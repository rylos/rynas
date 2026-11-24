//! Web Push to paired mobile devices (`docs/phase2/mobile-devices.md` §4): RFC 8291 message
//! encryption in the RFC 8188 `aes128gcm` content coding (one record, record size 4096,
//! padding delimiter 0x02) and RFC 8030 delivery to the device's push endpoint (UnifiedPush,
//! e.g. an ntfy server). No VAPID (RFC 8292) in v1.
//!
//! The subscription (endpoint, `p256dh`, `auth`) lives in the device store, not in the
//! engine: a `device` target names the device and [`PushDirectory`] finds it. A 404 or 410 of
//! the push service means the subscription is gone: the directory drops it.

use aes_gcm::aead::Aead;
use aes_gcm::{Aes128Gcm, KeyInit, Nonce};
use base64::Engine as _;
use hkdf::Hkdf;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use rynas_api_types::devices::{PUSH_MAX_ENDPOINT, PUSH_MAX_PAYLOAD, PushPayload, PushSubscription};
use sha2_hkdf::Sha256;

/// Record size announced in the header (one record carries the whole message).
pub const RECORD_SIZE: u32 = 4096;
/// How long the push service keeps an undelivered message.
pub const TTL_S: u32 = 86_400;

/// Where a `device` target's subscription comes from (rynasd's device store).
pub trait PushDirectory: Send + Sync {
    fn subscription(&self, device_id: &str) -> Option<PushSubscription>;
    /// The push service answered 404 or 410: the subscription no longer exists.
    fn gone(&self, device_id: &str);
}

fn b64u(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// A subscription checked: an https endpoint without user info, the user agent's public key
/// on the curve, a 16-byte auth secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub endpoint: reqwest::Url,
    pub p256dh: Vec<u8>,
    pub auth: Vec<u8>,
}

pub fn parse_subscription(s: &PushSubscription) -> Result<Subscription, &'static str> {
    parse_subscription_with(s, false)
}

/// [`parse_subscription`]; `loopback_http` also takes `http://127.0.0.1…` (the engine's tests
/// with a fake push service, [`super::EngineOptions::push_loopback_http`]).
pub fn parse_subscription_with(s: &PushSubscription, loopback_http: bool) -> Result<Subscription, &'static str> {
    if s.endpoint.len() > PUSH_MAX_ENDPOINT {
        return Err("endpoint: too long");
    }
    let endpoint = reqwest::Url::parse(s.endpoint.trim()).map_err(|_| "endpoint: not a URL")?;
    let loopback = loopback_http && endpoint.scheme() == "http" && endpoint.host_str() == Some("127.0.0.1");
    if (endpoint.scheme() != "https" && !loopback) || endpoint.host_str().is_none_or(str::is_empty) {
        return Err("endpoint: https only");
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        return Err("endpoint: no user name or password in the URL");
    }
    let p256dh = b64u(&s.p256dh).ok_or("p256dh: not base64url")?;
    if p256dh.len() != 65 || p256dh[0] != 0x04 || PublicKey::from_sec1_bytes(&p256dh).is_err() {
        return Err("p256dh: not an uncompressed P-256 point");
    }
    let auth = b64u(&s.auth).ok_or("auth: not base64url")?;
    if auth.len() != 16 {
        return Err("auth: not 16 bytes");
    }
    Ok(Subscription {
        endpoint,
        p256dh,
        auth,
    })
}

fn hkdf<const N: usize>(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(info, &mut out)
        .expect("HKDF output of at most 32 bytes");
    out
}

/// The content encryption key and nonce of RFC 8291 §3.3-3.4 / RFC 8188 §2.2-2.3.
fn keys(ecdh: &[u8], auth: &[u8], ua_public: &[u8], as_public: &[u8], salt: &[u8]) -> ([u8; 16], [u8; 12]) {
    let mut info = Vec::with_capacity(14 + 65 + 65);
    info.extend_from_slice(b"WebPush: info\0");
    info.extend_from_slice(ua_public);
    info.extend_from_slice(as_public);
    let ikm: [u8; 32] = hkdf(auth, ecdh, &info);
    (
        hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0"),
        hkdf(salt, &ikm, b"Content-Encoding: nonce\0"),
    )
}

/// Encrypts `plaintext` for the user agent key `ua_public` (65 bytes) and `auth` with the
/// application server key `as_secret` and `salt`: header (salt, record size, key id = the
/// server's public key) and the single record. Fixed keys only in tests: use [`encrypt`].
pub fn encrypt_with(
    ua_public: &[u8],
    auth: &[u8],
    plaintext: &[u8],
    as_secret: &[u8; 32],
    salt: &[u8; 16],
) -> Result<Vec<u8>, String> {
    let ua = PublicKey::from_sec1_bytes(ua_public).map_err(|_| "p256dh: not a P-256 point")?;
    let sk = SecretKey::from_slice(as_secret).map_err(|_| "server key out of range")?;
    let as_public = sk.public_key().to_encoded_point(false);
    let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), ua.as_affine());
    let (cek, nonce) = keys(shared.raw_secret_bytes(), auth, ua_public, as_public.as_bytes(), salt);
    let mut record = Vec::with_capacity(plaintext.len() + 1);
    record.extend_from_slice(plaintext);
    record.push(0x02);
    let ct = Aes128Gcm::new_from_slice(&cek)
        .expect("16-byte key")
        .encrypt(Nonce::from_slice(&nonce), record.as_slice())
        .map_err(|_| "encryption failed")?;
    if ct.len() > RECORD_SIZE as usize {
        return Err("message larger than one record".into());
    }
    let mut out = Vec::with_capacity(16 + 4 + 1 + 65 + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    out.push(as_public.as_bytes().len() as u8);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&ct);
    Ok(out)
}

/// [`encrypt_with`] with a fresh server key and salt (the OS random source).
pub fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    loop {
        let mut sk = [0u8; 32];
        getrandom::fill(&mut sk).map_err(|e| e.to_string())?;
        // a value outside the curve's order (probability ~2^-32) is drawn again
        if SecretKey::from_slice(&sk).is_ok() {
            return encrypt_with(ua_public, auth, plaintext, &sk, &salt);
        }
    }
}

/// The user agent's side: decrypts a message for the key `ua_secret` (tests, the fake push
/// server).
pub fn decrypt(ua_secret: &[u8; 32], auth: &[u8], body: &[u8]) -> Result<Vec<u8>, String> {
    if body.len() < 21 {
        return Err("too short".into());
    }
    let salt = &body[..16];
    let idlen = body[20] as usize;
    if body.len() < 21 + idlen {
        return Err("too short".into());
    }
    let as_public = &body[21..21 + idlen];
    let ct = &body[21 + idlen..];
    let sk = SecretKey::from_slice(ua_secret).map_err(|_| "bad key")?;
    let ua_public = sk.public_key().to_encoded_point(false);
    let peer = PublicKey::from_sec1_bytes(as_public).map_err(|_| "bad key id")?;
    let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer.as_affine());
    let (cek, nonce) = keys(shared.raw_secret_bytes(), auth, ua_public.as_bytes(), as_public, salt);
    let mut pt = Aes128Gcm::new_from_slice(&cek)
        .expect("16-byte key")
        .decrypt(Nonce::from_slice(&nonce), ct)
        .map_err(|_| "not authentic")?;
    while pt.last() == Some(&0) {
        pt.pop();
    }
    if pt.pop() != Some(0x02) {
        return Err("no final record delimiter".into());
    }
    Ok(pt)
}

/// The JSON plaintext of `p`, at most [`PUSH_MAX_PAYLOAD`] bytes: the body (then the title)
/// shortened to fit, with an ellipsis.
pub fn payload_json(mut p: PushPayload) -> Vec<u8> {
    loop {
        let j = serde_json::to_vec(&p).unwrap_or_default();
        if j.len() <= PUSH_MAX_PAYLOAD {
            return j;
        }
        let excess = j.len() - PUSH_MAX_PAYLOAD;
        let field = if !p.body.is_empty() { &mut p.body } else { &mut p.title };
        if field.is_empty() {
            return j;
        }
        let mut cut = field.trim_end_matches('…').to_string();
        let mut removed = 0;
        while removed < excess + 3 && !cut.is_empty() {
            removed += cut.pop().map(char::len_utf8).unwrap_or(0);
        }
        if !cut.is_empty() {
            cut.push('…');
        }
        *field = cut;
    }
}

/// `Urgency` of a severity (RFC 8030 §5.3): `high` for errors and critical events.
pub fn urgency(severity: &str) -> &'static str {
    match severity {
        "error" | "critical" => "high",
        _ => "normal",
    }
}

/// What the push service did with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Accepted,
    /// 404 or 410: the subscription no longer exists.
    Gone,
}

/// Posts one encrypted message (RFC 8030 §5). Errors carry no URL (the endpoint is a
/// capability).
pub async fn post(
    http: &reqwest::Client,
    sub: &Subscription,
    plaintext: &[u8],
    urgency: &str,
) -> Result<Delivery, String> {
    let body = encrypt(&sub.p256dh, &sub.auth, plaintext)?;
    let r = http
        .post(sub.endpoint.clone())
        .header(reqwest::header::CONTENT_ENCODING, "aes128gcm")
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .header("TTL", TTL_S.to_string())
        .header("Urgency", urgency)
        .body(body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    match r.status().as_u16() {
        200..=299 => Ok(Delivery::Accepted),
        404 | 410 => Ok(Delivery::Gone),
        s => Err(format!("push service: HTTP {s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        b64u(s).unwrap()
    }

    fn arr<const N: usize>(v: Vec<u8>) -> [u8; N] {
        v.try_into().unwrap()
    }

    /// RFC 8291 Appendix A: the example message, with its fixed keys and salt.
    #[test]
    fn rfc8291_appendix_a() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let as_secret: [u8; 32] = arr(d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw"));
        let ua_secret: [u8; 32] = arr(d("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"));
        let ua_public = d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = arr(d("DGv6ra1nlYgDCS1FRnbzlw"));
        let expected = d(
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN",
        );
        let body = encrypt_with(&ua_public, &auth, plaintext, &as_secret, &salt).unwrap();
        assert_eq!(body, expected);
        assert_eq!(decrypt(&ua_secret, &auth, &expected).unwrap(), plaintext);
    }

    #[test]
    fn round_trip_with_fresh_keys() {
        let ua_secret: [u8; 32] = arr(d("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94"));
        let ua_public = SecretKey::from_slice(&ua_secret)
            .unwrap()
            .public_key()
            .to_encoded_point(false)
            .as_bytes()
            .to_vec();
        let auth = [7u8; 16];
        let a = encrypt(&ua_public, &auth, b"hello").unwrap();
        let b = encrypt(&ua_public, &auth, b"hello").unwrap();
        assert_ne!(a, b, "fresh salt and key each time");
        assert_eq!(&a[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(a[20], 65);
        assert_eq!(decrypt(&ua_secret, &auth, &a).unwrap(), b"hello");
        assert!(decrypt(&ua_secret, &[8u8; 16], &a).is_err(), "another auth secret");
        let mut bad = a.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decrypt(&ua_secret, &auth, &bad).is_err());
        // the largest payload still fits one record
        assert!(encrypt(&ua_public, &auth, &[b'x'; PUSH_MAX_PAYLOAD]).is_ok());
    }

    #[test]
    fn subscriptions_are_checked() {
        let ok = PushSubscription {
            endpoint: "https://ntfy.example.org/upABC?up=1".into(),
            p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
            auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
        };
        assert!(parse_subscription(&ok).is_ok());
        for (endpoint, p256dh, auth) in [
            ("http://ntfy.example.org/up", ok.p256dh.as_str(), ok.auth.as_str()),
            ("https://user:pw@ntfy.example.org/up", ok.p256dh.as_str(), ok.auth.as_str()),
            ("not a url", ok.p256dh.as_str(), ok.auth.as_str()),
            (ok.endpoint.as_str(), "BAAA", ok.auth.as_str()),
            (ok.endpoint.as_str(), ok.p256dh.as_str(), "AAAA"),
        ] {
            let s = PushSubscription {
                endpoint: endpoint.into(),
                p256dh: p256dh.into(),
                auth: auth.into(),
            };
            assert!(parse_subscription(&s).is_err(), "{endpoint} {p256dh} {auth}");
        }
        // a point that is not on the curve
        let mut off = d(&ok.p256dh);
        off[64] ^= 1;
        let s = PushSubscription {
            p256dh: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&off),
            ..ok
        };
        assert!(parse_subscription(&s).is_err());
    }

    #[test]
    fn payloads_fit_in_3_kib() {
        let p = PushPayload {
            v: 1,
            id: 7,
            title: "Disk failing".into(),
            body: "é\"x".repeat(2000),
            severity: "critical".into(),
            ..Default::default()
        };
        let j = payload_json(p);
        assert!(j.len() <= PUSH_MAX_PAYLOAD, "{}", j.len());
        let back: PushPayload = serde_json::from_slice(&j).unwrap();
        assert!(back.body.ends_with('…'));
        assert_eq!((back.id, back.title.as_str()), (7, "Disk failing"));
        assert_eq!(urgency("critical"), "high");
        assert_eq!(urgency("warning"), "normal");
    }
}
