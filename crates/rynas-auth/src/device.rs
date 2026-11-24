//! Mobile devices (`docs/phase2/mobile-devices.md`): the crypto of the pairing protocol and
//! the pairings waiting for a phone, in memory.
//!
//! - A phone proves it holds its P-256 key by signing UTF-8 messages (ECDSA with SHA-256,
//!   ASN.1 DER, b64u) checked here with `ring`: the claim of a pairing ([`pair_message`]) and
//!   every refresh ([`refresh_message`]).
//! - [`Pairings`]: codes of 32 random bytes kept only as SHA-256, single use, 300 s, at most
//!   three open per user; the claim token likewise; the device's tokens wait here (memory
//!   only) until the phone's poll takes them, once.
//!
//! Nothing here logs a code, token or key.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rynas_api_types::devices::{
    ClaimRequest, ClaimResponse, DeviceInfo, MAX_CLOCK_SKEW_S, MAX_OPEN_PAIRINGS, PAIRING_TTL_S,
    PROTOCOL_VERSION, PairingClaim, PairingState, PairingStatus, PollApproved, clean_text,
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{Clock, random_bytes, random_token};

/// Undelivered approved pairings are kept this long after their expiry, for a phone whose
/// last poll was slow.
pub const DELIVERY_GRACE_MS: u64 = 120_000;

/// The DER prefix of a P-256 SubjectPublicKeyInfo (id-ecPublicKey, prime256v1, BIT STRING of
/// 66 bytes): the uncompressed point follows.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

pub fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url, padded or not.
pub fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

pub fn sha256(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

/// `b64u(SHA-256(text))`: how a token is named in signed messages and stored.
pub fn sha_b64u(text: &str) -> String {
    b64u(&sha256(text.as_bytes()))
}

/// Constant-time comparison of two digests or tokens.
pub fn ct_eq_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// The uncompressed point of a P-256 SubjectPublicKeyInfo, `None` for anything else.
pub fn p256_point(spki: &[u8]) -> Option<&[u8]> {
    (spki.len() == P256_SPKI_PREFIX.len() + 65
        && spki[..P256_SPKI_PREFIX.len()] == P256_SPKI_PREFIX
        && spki[P256_SPKI_PREFIX.len()] == 0x04)
        .then(|| &spki[P256_SPKI_PREFIX.len()..])
}

/// The SubjectPublicKeyInfo of an uncompressed P-256 point (tests and tools).
pub fn spki_of_point(point: &[u8]) -> Vec<u8> {
    let mut v = P256_SPKI_PREFIX.to_vec();
    v.extend_from_slice(point);
    v
}

/// `b64u(SHA-256(DER SPKI))`.
pub fn key_hash(spki: &[u8]) -> String {
    b64u(&sha256(spki))
}

/// Whether `sig_b64u` (ECDSA P-256 SHA-256, ASN.1 DER) signs `msg` with the key `spki`.
pub fn verify(spki: &[u8], msg: &str, sig_b64u: &str) -> bool {
    let (Some(point), Some(sig)) = (p256_point(spki), b64u_decode(sig_b64u)) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, point)
        .verify(msg.as_bytes(), &sig)
        .is_ok()
}

/// The message a phone signs to claim a pairing.
pub fn pair_message(sid: &str, code: &str, key_hash: &str, ts: i64) -> String {
    format!("rynas-pair-v1\n{sid}\n{code}\n{key_hash}\n{ts}")
}

/// The message a device signs for a refresh (`refresh_sha` = `b64u(SHA-256(refresh token))`).
pub fn refresh_message(sid: &str, device_id: &str, ts: i64, refresh_sha: &str) -> String {
    format!("rynas-refresh-v1\n{sid}\n{device_id}\n{ts}\n{refresh_sha}")
}

/// Whether a signed `ts` (Unix seconds) is within [`MAX_CLOCK_SKEW_S`] of `now_ms`.
pub fn ts_ok(ts: i64, now_ms: u64) -> bool {
    let now = (now_ms / 1000) as i64;
    ts.checked_sub(now)
        .is_some_and(|d| d.unsigned_abs() <= MAX_CLOCK_SKEW_S)
}

/// The 6 digits both screens show: `SHA-256(code ‖ DER SPKI)` as an unsigned big-endian
/// integer, mod 10⁶, zero-padded, `"482 913"`.
pub fn compare_code(code: &str, spki: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(code.as_bytes());
    h.update(spki);
    let n = h
        .finalize()
        .iter()
        .fold(0u64, |r, b| (r * 256 + u64::from(*b)) % 1_000_000);
    let s = format!("{n:06}");
    format!("{} {}", &s[..3], &s[3..])
}

/// A random id with a prefix: `p_` pairings, `d_` devices (12 random bytes, hex).
pub fn new_id(prefix: &str) -> String {
    let b = random_bytes::<12>();
    let mut s = String::from(prefix);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// The phone behind a claimed pairing.
#[derive(Debug, Clone)]
pub struct Claim {
    pub device: DeviceInfo,
    /// DER SPKI.
    pub public_key: Vec<u8>,
    pub compare_code: String,
    pub ip: String,
    pub claimed_ms: u64,
}

#[derive(Debug)]
struct Pairing {
    user: String,
    realm: String,
    code_sha: [u8; 32],
    expires_ms: u64,
    state: PairingState,
    claim: Option<Claim>,
    claim_sha: Option<[u8; 32]>,
    /// Set while the approval registers the device with rynasd.
    approving: bool,
    /// The approved device's tokens until the phone's poll takes them.
    approved: Option<PollApproved>,
    /// The approved device (kept after delivery to tell the two apart in [`Pairings::purge`]).
    device_id: Option<String>,
}

impl Pairing {
    fn state_at(&self, now: u64) -> PairingState {
        match self.state {
            PairingState::Waiting | PairingState::Claimed if now >= self.expires_ms => {
                PairingState::Expired
            }
            s => s,
        }
    }

    fn owned_by(&self, user: &str, realm: &str) -> bool {
        self.user == user && self.realm == realm
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingError {
    /// The user has [`MAX_OPEN_PAIRINGS`] open already.
    TooMany,
    /// No such pairing for this user.
    NotFound,
    /// Not in a state that allows it (approve when not claimed or expired, …).
    WrongState(PairingState),
}

/// Why a claim was refused ([`Pairings::claim`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimError {
    /// Another protocol version or NAS id, an oversized code, not a P-256 key: no secret was
    /// tried, nothing counts against the client.
    Malformed,
    /// An unknown, used or expired code, or a signature of another key: a guess, counted.
    Invalid,
    /// A valid code signed by the claiming key, but its `ts` is more than
    /// [`MAX_CLOCK_SKEW_S`] away from the NAS' clock: the phone's clock is wrong.
    ClockSkew,
}

/// What the phone's poll finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    Unknown,
    Pending,
    Rejected,
    Expired,
    Approved(Box<PollApproved>),
}

/// The pairings of every user (memory only: a restart of rynas-web ends them).
pub struct Pairings {
    clock: Arc<dyn Clock>,
    map: Mutex<HashMap<String, Pairing>>,
    changed: tokio::sync::Notify,
}

impl Pairings {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            map: Mutex::new(HashMap::new()),
            changed: tokio::sync::Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pairing>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Wakes the phones' polls (a claim, an approval, a rejection).
    pub fn changed(&self) -> &tokio::sync::Notify {
        &self.changed
    }

    /// A new pairing of `user`: its id, the code (only in the QR code, never stored) and the
    /// expiry.
    pub fn create(&self, user: &str, realm: &str) -> Result<(String, String, u64), PairingError> {
        let now = self.clock.now_ms();
        let mut map = self.lock();
        let open = map
            .values()
            .filter(|p| p.owned_by(user, realm))
            .filter(|p| matches!(p.state_at(now), PairingState::Waiting | PairingState::Claimed))
            .count();
        if open >= MAX_OPEN_PAIRINGS {
            return Err(PairingError::TooMany);
        }
        let code = random_token();
        let id = new_id("p_");
        let expires_ms = now + PAIRING_TTL_S * 1000;
        map.insert(
            id.clone(),
            Pairing {
                user: user.into(),
                realm: realm.into(),
                code_sha: sha256(code.as_bytes()),
                expires_ms,
                state: PairingState::Waiting,
                claim: None,
                claim_sha: None,
                approving: false,
                approved: None,
                device_id: None,
            },
        );
        Ok((id, code, expires_ms))
    }

    /// The pairing `id` of `user`, as the web interface sees it.
    pub fn status(&self, id: &str, user: &str, realm: &str) -> Option<PairingStatus> {
        let now = self.clock.now_ms();
        let map = self.lock();
        let p = map.get(id).filter(|p| p.owned_by(user, realm))?;
        Some(PairingStatus {
            state: p.state_at(now),
            expires_ms: p.expires_ms,
            claim: p.claim.as_ref().map(|c| PairingClaim {
                device: c.device.clone(),
                compare_code: c.compare_code.clone(),
                ip: c.ip.clone(),
                claimed_ms: c.claimed_ms,
            }),
        })
    }

    /// A phone's claim. [`ClaimError::Invalid`] (unknown, used or expired code, bad
    /// signature) and [`ClaimError::Malformed`] (another version or NAS' id, a bad key) are
    /// answered with one generic error; [`ClaimError::ClockSkew`] only to a phone that holds
    /// a valid code and signed it, and the code stays usable.
    pub fn claim(&self, sid: &str, req: &ClaimRequest, ip: &str) -> Result<ClaimResponse, ClaimError> {
        let now = self.clock.now_ms();
        if req.v != PROTOCOL_VERSION || !ct_eq_bytes(req.sid.as_bytes(), sid.as_bytes()) {
            return Err(ClaimError::Malformed);
        }
        if req.code.len() > 128 {
            return Err(ClaimError::Malformed);
        }
        let spki = b64u_decode(&req.public_key)
            .filter(|k| p256_point(k).is_some())
            .ok_or(ClaimError::Malformed)?;
        let code_sha = sha256(req.code.as_bytes());
        let mut map = self.lock();
        // every pairing is compared, in constant time each
        let mut found = None;
        for (id, p) in map.iter() {
            if ct_eq_bytes(&p.code_sha, &code_sha) {
                found = Some(id.clone());
            }
        }
        let p = found
            .and_then(|id| map.get_mut(&id))
            .ok_or(ClaimError::Invalid)?;
        if p.state_at(now) != PairingState::Waiting {
            return Err(ClaimError::Invalid);
        }
        let msg = pair_message(sid, &req.code, &key_hash(&spki), req.ts);
        if !verify(&spki, &msg, &req.sig) {
            return Err(ClaimError::Invalid);
        }
        if !ts_ok(req.ts, now) {
            return Err(ClaimError::ClockSkew);
        }
        let compare = compare_code(&req.code, &spki);
        let token = random_token();
        p.state = PairingState::Claimed;
        p.claim_sha = Some(sha256(token.as_bytes()));
        p.claim = Some(Claim {
            device: DeviceInfo {
                name: clean_text(&req.device.name),
                model: clean_text(&req.device.model),
                os: clean_text(&req.device.os),
                app_version: clean_text(&req.device.app_version),
            },
            public_key: spki,
            compare_code: compare.clone(),
            ip: ip.into(),
            claimed_ms: now,
        });
        let expires_ms = p.expires_ms;
        drop(map);
        self.changed.notify_waiters();
        Ok(ClaimResponse {
            claim_token: token,
            compare_code: compare,
            poll_after_ms: 1500,
            expires_ms,
        })
    }

    /// Starts approving the claimed pairing `id` of `user`: returns the claim; the pairing
    /// cannot be approved twice meanwhile. End with [`Pairings::approved`] or
    /// [`Pairings::approve_failed`].
    pub fn begin_approve(&self, id: &str, user: &str, realm: &str) -> Result<Claim, PairingError> {
        let now = self.clock.now_ms();
        let mut map = self.lock();
        let p = map
            .get_mut(id)
            .filter(|p| p.owned_by(user, realm))
            .ok_or(PairingError::NotFound)?;
        let state = p.state_at(now);
        if state != PairingState::Claimed || p.approving {
            return Err(PairingError::WrongState(state));
        }
        p.approving = true;
        p.claim.clone().ok_or(PairingError::WrongState(state))
    }

    pub fn approve_failed(&self, id: &str) {
        if let Some(p) = self.lock().get_mut(id) {
            p.approving = false;
        }
    }

    /// The device exists: its tokens wait for the phone's poll.
    pub fn approved(&self, id: &str, answer: PollApproved) {
        if let Some(p) = self.lock().get_mut(id) {
            p.approving = false;
            p.state = PairingState::Approved;
            p.device_id = Some(answer.device_id.clone());
            p.approved = Some(answer);
        }
        self.changed.notify_waiters();
    }

    /// The user refused the claim (or cancelled a waiting pairing).
    pub fn reject(&self, id: &str, user: &str, realm: &str) -> Result<(), PairingError> {
        let now = self.clock.now_ms();
        let mut map = self.lock();
        let p = map
            .get_mut(id)
            .filter(|p| p.owned_by(user, realm))
            .ok_or(PairingError::NotFound)?;
        let state = p.state_at(now);
        if !matches!(state, PairingState::Waiting | PairingState::Claimed) || p.approving {
            return Err(PairingError::WrongState(state));
        }
        p.state = PairingState::Rejected;
        drop(map);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Cancels the pairing `id` of `user`: a waiting one goes away (its code stops working),
    /// a claimed one is rejected (the phone hears so).
    pub fn cancel(&self, id: &str, user: &str, realm: &str) -> Result<(), PairingError> {
        let now = self.clock.now_ms();
        let waiting = {
            let map = self.lock();
            let p = map
                .get(id)
                .filter(|p| p.owned_by(user, realm))
                .ok_or(PairingError::NotFound)?;
            p.state_at(now) == PairingState::Waiting
        };
        if waiting {
            self.lock().remove(id);
            Ok(())
        } else {
            self.reject(id, user, realm)
        }
    }

    /// The phone's poll with its claim token. The approved answer is given out once.
    pub fn poll(&self, claim_token: &str) -> PollOutcome {
        let now = self.clock.now_ms();
        let sha = sha256(claim_token.as_bytes());
        let mut map = self.lock();
        let mut found = None;
        for (id, p) in map.iter() {
            if p.claim_sha.is_some_and(|c| ct_eq_bytes(&c, &sha)) {
                found = Some(id.clone());
            }
        }
        let Some(p) = found.and_then(|id| map.get_mut(&id)) else {
            return PollOutcome::Unknown;
        };
        match p.state_at(now) {
            PairingState::Approved => match p.approved.take() {
                Some(a) => {
                    p.claim_sha = None;
                    PollOutcome::Approved(Box::new(a))
                }
                None => PollOutcome::Unknown,
            },
            PairingState::Rejected => PollOutcome::Rejected,
            PairingState::Expired => PollOutcome::Expired,
            PairingState::Claimed | PairingState::Waiting => PollOutcome::Pending,
        }
    }

    /// Drops finished pairings; returns the devices (id, owner) that were approved but never
    /// picked up by their phone (the caller revokes them: nobody holds their tokens).
    pub fn purge(&self) -> Vec<(String, String)> {
        let now = self.clock.now_ms();
        let mut orphans = Vec::new();
        self.lock().retain(|_, p| {
            let keep = now < p.expires_ms + DELIVERY_GRACE_MS || p.approving;
            if !keep
                && p.approved.is_some()
                && let Some(d) = &p.device_id
            {
                orphans.push((d.clone(), p.user.clone()));
            }
            keep
        });
        orphans
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ManualClock;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};

    /// A software P-256 key, like the phone's.
    pub struct Phone {
        key: EcdsaKeyPair,
        rng: SystemRandom,
    }

    impl Phone {
        pub fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { key, rng }
        }
        pub fn spki(&self) -> Vec<u8> {
            spki_of_point(self.key.public_key().as_ref())
        }
        pub fn sign(&self, msg: &str) -> String {
            b64u(self.key.sign(&self.rng, msg.as_bytes()).unwrap().as_ref())
        }
        pub fn claim(&self, sid: &str, code: &str, ts: i64) -> ClaimRequest {
            let spki = self.spki();
            ClaimRequest {
                v: 1,
                sid: sid.into(),
                code: code.into(),
                public_key: b64u(&spki),
                device: DeviceInfo {
                    name: "Galaxy S25".into(),
                    model: "SM-S931B".into(),
                    os: "Android 16".into(),
                    app_version: "0.1.0".into(),
                },
                ts,
                sig: self.sign(&pair_message(sid, code, &key_hash(&spki), ts)),
            }
        }
    }

    const T0: u64 = 1_800_000_000_000;

    #[test]
    fn signatures_and_keys() {
        let phone = Phone::new();
        let spki = phone.spki();
        assert_eq!(spki.len(), 91);
        assert!(p256_point(&spki).is_some());
        assert!(p256_point(&spki[1..]).is_none());
        let sig = phone.sign("hello");
        assert!(verify(&spki, "hello", &sig));
        assert!(!verify(&spki, "hellO", &sig));
        assert!(!verify(&Phone::new().spki(), "hello", &sig));
        assert!(!verify(&spki, "hello", "not base64!"));
        assert_eq!(key_hash(&spki).len(), 43);
    }

    #[test]
    fn compare_code_is_six_digits_of_the_whole_hash() {
        // the mock NAS: int.from_bytes(sha256(code + spki), "big") % 1_000_000
        // (values from Python: int.from_bytes(hashlib.sha256(…).digest(), "big") % 10**6)
        assert_eq!(compare_code("abc", b""), "089 965", "zero-padded");
        let spki: Vec<u8> = (0..10).collect();
        assert_eq!(compare_code("code", &spki), "538 684");
    }

    #[test]
    fn timestamps() {
        let now = T0;
        let s = (T0 / 1000) as i64;
        assert!(ts_ok(s, now) && ts_ok(s - 120, now) && ts_ok(s + 120, now));
        assert!(!ts_ok(s - 121, now) && !ts_ok(s + 121, now) && !ts_ok(i64::MIN, now));
    }

    fn approved(id: &str) -> PollApproved {
        PollApproved {
            device_id: id.into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_pairing_from_code_to_tokens() {
        let clock = Arc::new(ManualClock::new(T0));
        let ps = Pairings::new(clock.clone());
        let (id, code, exp) = ps.create("alice", "pam").unwrap();
        assert_eq!(exp, T0 + 300_000);
        assert_eq!(ps.status(&id, "alice", "pam").unwrap().state, PairingState::Waiting);
        assert!(ps.status(&id, "bob", "pam").is_none(), "only the owner sees it");
        let phone = Phone::new();
        let ts = (T0 / 1000) as i64;
        // wrong NAS id: malformed; wrong code, bad signature: a guess
        let claim = |req: &ClaimRequest| ps.claim("sid", req, "1.2.3.4");
        assert_eq!(claim(&phone.claim("other", &code, ts)).unwrap_err(), ClaimError::Malformed);
        assert_eq!(claim(&phone.claim("sid", "nope", ts)).unwrap_err(), ClaimError::Invalid);
        let mut forged = phone.claim("sid", &code, ts);
        forged.public_key = b64u(&Phone::new().spki());
        assert_eq!(claim(&forged).unwrap_err(), ClaimError::Invalid);
        // the right code, signed, with the phone's clock 500 s off: told so, the code stays
        assert_eq!(claim(&phone.claim("sid", &code, ts - 500)).unwrap_err(), ClaimError::ClockSkew);
        // a skewed claim of a wrong code is still only a guess
        assert_eq!(claim(&phone.claim("sid", "nope", ts + 500)).unwrap_err(), ClaimError::Invalid);
        let r = claim(&phone.claim("sid", &code, ts)).unwrap();
        assert_eq!(r.compare_code, compare_code(&code, &phone.spki()));
        // single use
        assert_eq!(claim(&phone.claim("sid", &code, ts)).unwrap_err(), ClaimError::Invalid);
        let st = ps.status(&id, "alice", "pam").unwrap();
        assert_eq!(st.state, PairingState::Claimed);
        assert_eq!(st.claim.unwrap().device.model, "SM-S931B");
        assert_eq!(ps.poll(&r.claim_token), PollOutcome::Pending);
        assert_eq!(ps.poll("garbage"), PollOutcome::Unknown);
        // approval: once
        assert!(ps.begin_approve(&id, "bob", "pam").is_err());
        let claim = ps.begin_approve(&id, "alice", "pam").unwrap();
        assert_eq!(claim.public_key, phone.spki());
        assert!(matches!(ps.begin_approve(&id, "alice", "pam"), Err(PairingError::WrongState(_))));
        ps.approved(&id, approved("d_1"));
        match ps.poll(&r.claim_token) {
            PollOutcome::Approved(a) => assert_eq!(a.device_id, "d_1"),
            o => panic!("{o:?}"),
        }
        assert_eq!(ps.poll(&r.claim_token), PollOutcome::Unknown, "tokens given once");
        clock.advance_ms(300_000 + DELIVERY_GRACE_MS);
        assert!(ps.purge().is_empty(), "delivered: not an orphan");
        assert!(ps.status(&id, "alice", "pam").is_none());
    }

    #[test]
    fn limits_expiry_rejection_and_orphans() {
        let clock = Arc::new(ManualClock::new(T0));
        let ps = Pairings::new(clock.clone());
        let ids: Vec<_> = (0..3).map(|_| ps.create("alice", "pam").unwrap()).collect();
        assert_eq!(ps.create("alice", "pam"), Err(PairingError::TooMany));
        assert!(ps.create("bob", "pam").is_ok());
        ps.cancel(&ids[0].0, "alice", "pam").unwrap();
        assert!(ps.create("alice", "pam").is_ok());
        // rejected claim
        let phone = Phone::new();
        let ts = (T0 / 1000) as i64;
        let r = ps.claim("s", &phone.claim("s", &ids[1].1, ts), "ip").unwrap();
        ps.reject(&ids[1].0, "alice", "pam").unwrap();
        assert_eq!(ps.poll(&r.claim_token), PollOutcome::Rejected);
        // expired before approval
        let r2 = ps.claim("s", &phone.claim("s", &ids[2].1, ts), "ip").unwrap();
        clock.advance_ms(300_000);
        assert_eq!(ps.poll(&r2.claim_token), PollOutcome::Expired);
        assert!(ps.begin_approve(&ids[2].0, "alice", "pam").is_err());
        // an expired code cannot be claimed
        let (_, code, _) = ps.create("carol", "pam").unwrap();
        clock.advance_ms(300_000);
        let ts = (clock.now_ms() / 1000) as i64;
        assert_eq!(ps.claim("s", &phone.claim("s", &code, ts), "ip").unwrap_err(), ClaimError::Invalid);
        // approved but never picked up: an orphan
        let (id, code, _) = ps.create("dave", "pam").unwrap();
        ps.claim("s", &phone.claim("s", &code, ts), "ip").unwrap();
        ps.begin_approve(&id, "dave", "pam").unwrap();
        ps.approved(&id, approved("d_9"));
        clock.advance_ms(300_000 + DELIVERY_GRACE_MS);
        assert_eq!(ps.purge(), vec![("d_9".to_string(), "dave".to_string())]);
    }
}
