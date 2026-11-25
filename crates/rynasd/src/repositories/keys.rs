//! The OpenPGP key of a third-party repository: read (armored or binary), described
//! (fingerprints, user ids, creation and expiry) for the user to confirm, and written armored
//! as `/etc/apt/keyrings/<name>.asc`.
//!
//! Only public keys, user ids, user attributes, signatures and subkeys are accepted (RFC 9580
//! §10.1): a secret key or anything else is refused. The signatures are not verified here
//! (apt's gpgv does it on every update); the user's confirmation of the fingerprint is the
//! trust step, as with any key added by hand.

use rynas_api_types::repositories::{KEY_MAX_BYTES, KeyInfo};

const BEGIN: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
const END: &str = "-----END PGP PUBLIC KEY BLOCK-----";

/// A packet: tag, body, and the whole packet with its header.
struct Packet<'a> {
    tag: u8,
    body: &'a [u8],
    raw: &'a [u8],
}

fn packets(bin: &[u8]) -> Result<Vec<Packet<'_>>, String> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let byte = |i: usize| bin.get(i).copied().map(usize::from).ok_or("truncated key");
    while at < bin.len() {
        let first = bin[at];
        if first & 0x80 == 0 {
            return Err("not an OpenPGP key".into());
        }
        let (tag, len, hdr) = if first & 0x40 != 0 {
            let tag = first & 0x3f;
            match byte(at + 1)? {
                o @ 0..=191 => (tag, o, 2),
                o @ 192..=223 => (tag, ((o - 192) << 8) + byte(at + 2)? + 192, 3),
                255 => {
                    let b = bin.get(at + 2..at + 6).ok_or("truncated key")?;
                    (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 6)
                }
                _ => return Err("partial packet lengths in a key".into()),
            }
        } else {
            let tag = (first >> 2) & 0x0f;
            match first & 3 {
                0 => (tag, byte(at + 1)?, 2),
                1 => {
                    let b = bin.get(at + 1..at + 3).ok_or("truncated key")?;
                    (tag, u16::from_be_bytes([b[0], b[1]]) as usize, 3)
                }
                2 => {
                    let b = bin.get(at + 1..at + 5).ok_or("truncated key")?;
                    (tag, u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize, 5)
                }
                _ => return Err("indeterminate packet length".into()),
            }
        };
        let end = (at + hdr).checked_add(len).ok_or("truncated key")?;
        let body = bin.get(at + hdr..end).ok_or("truncated key")?;
        out.push(Packet { tag, body, raw: &bin[at..end] });
        at = end;
    }
    Ok(out)
}

/// The binary keyring of an armored text (one or more public key blocks) or of binary bytes.
pub fn dearmor(input: &[u8]) -> Result<Vec<u8>, String> {
    use base64::Engine;
    if input.len() > KEY_MAX_BYTES {
        return Err(format!("the key is larger than {} KiB", KEY_MAX_BYTES / 1024));
    }
    if input.first().is_some_and(|b| b & 0x80 != 0) {
        return Ok(input.to_vec());
    }
    let text = std::str::from_utf8(input).map_err(|_| "neither an armored nor a binary OpenPGP key")?;
    if text.contains("PRIVATE KEY BLOCK") {
        return Err("this is a private key: only the public key may be given".into());
    }
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(BEGIN) {
        let after = &rest[i + BEGIN.len()..];
        let j = after.find(END).ok_or("an armored key block without its END line")?;
        let block = &after[..j];
        rest = &after[j + END.len()..];
        let mut lines = block.lines().map(str::trim).skip_while(|l| l.is_empty()).peekable();
        // armor headers (`Version:`, `Comment:`) up to an empty line
        if lines.peek().is_some_and(|l| l.contains(": ")) {
            for l in lines.by_ref() {
                if l.is_empty() {
                    break;
                }
            }
        }
        let b64: String = lines.filter(|l| !l.is_empty() && !l.starts_with('=')).collect();
        let data = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| format!("bad base64 in the key: {e}"))?;
        out.extend(data);
    }
    if out.is_empty() {
        return Err("no OpenPGP public key block found".into());
    }
    Ok(out)
}

/// Tags a public keyring may hold: signature, public key, trust, user id, public subkey,
/// user attribute.
const ALLOWED: &[u8] = &[2, 6, 12, 13, 14, 17];

fn be32(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 4).map(|x| u64::from(u32::from_be_bytes([x[0], x[1], x[2], x[3]])))
}

fn fingerprint(body: &[u8]) -> Result<String, String> {
    use sha1::Digest as _;
    let hex = |d: &[u8]| d.iter().map(|b| format!("{b:02X}")).collect::<String>();
    match body.first() {
        Some(4) => {
            let len = u16::try_from(body.len()).map_err(|_| "key packet too long")?;
            let mut h = sha1::Sha1::new();
            h.update([0x99]);
            h.update(len.to_be_bytes());
            h.update(body);
            Ok(hex(&h.finalize()))
        }
        Some(v @ (5 | 6)) => {
            use sha2::Digest as _;
            let len = u32::try_from(body.len()).map_err(|_| "key packet too long")?;
            let mut h = sha2::Sha256::new();
            h.update([if *v == 6 { 0x9b } else { 0x9a }]);
            h.update(len.to_be_bytes());
            h.update(body);
            Ok(hex(&h.finalize()))
        }
        _ => Err("an OpenPGP key older than version 4".into()),
    }
}

/// `(type, creation, key expiration)` of a signature (its hashed subpackets).
fn sig_times(body: &[u8]) -> Option<(u8, Option<u64>, Option<u64>)> {
    let (ty, start, len) = match body.first()? {
        4 => (*body.get(1)?, 6usize, u16::from_be_bytes([*body.get(4)?, *body.get(5)?]) as usize),
        6 => (*body.get(1)?, 8usize, be32(body, 4)? as usize),
        _ => return None,
    };
    let sub = body.get(start..start.checked_add(len)?)?;
    let (mut created, mut expires) = (None, None);
    let mut i = 0usize;
    while i < sub.len() {
        let o = sub[i] as usize;
        let (l, h) = match o {
            0..=191 => (o, 1),
            192..=254 => (((o - 192) << 8) + *sub.get(i + 1)? as usize + 192, 2),
            _ => (be32(sub, i + 1)? as usize, 5),
        };
        let t = *sub.get(i + h)? & 0x7f;
        let data = sub.get(i + h + 1..i + h + l)?;
        match t {
            2 => created = be32(data, 0),
            9 => expires = be32(data, 0),
            _ => {}
        }
        i += h + l;
    }
    Some((ty, created, expires))
}

/// The keys of a binary keyring. `now_ms` decides `expired`.
pub fn describe(bin: &[u8], now_ms: u64) -> Result<Vec<KeyInfo>, String> {
    let ps = packets(bin)?;
    if let Some(p) = ps.iter().find(|p| !ALLOWED.contains(&p.tag)) {
        return Err(match p.tag {
            5 | 7 => "this is a private key: only the public key may be given".to_string(),
            t => format!("the key holds an OpenPGP packet of type {t}, not part of a public key"),
        });
    }
    if ps.first().map(|p| p.tag) != Some(6) {
        return Err("the data does not start with a public key".into());
    }
    let mut out: Vec<KeyInfo> = Vec::new();
    // the newest self-certification's creation time per key
    let mut newest: Vec<Option<u64>> = Vec::new();
    let mut created_s = 0u64;
    let mut in_subkey = false;
    for p in &ps {
        match p.tag {
            6 => {
                created_s = be32(p.body, 1).unwrap_or(0);
                out.push(KeyInfo { fingerprint: fingerprint(p.body)?, created_ms: Some(created_s * 1000), ..Default::default() });
                newest.push(None);
                in_subkey = false;
            }
            14 => {
                if let Some(k) = out.last_mut() {
                    k.subkeys += 1;
                }
                in_subkey = true;
            }
            13 => {
                in_subkey = false;
                if let Some(k) = out.last_mut() {
                    k.uids.push(String::from_utf8_lossy(p.body).into_owned());
                }
            }
            2 if !in_subkey => {
                let Some((ty, c, exp)) = sig_times(p.body) else { continue };
                // certifications of a user id (0x10-0x13) and direct-key signatures (0x1f)
                if !(0x10..=0x13).contains(&ty) && ty != 0x1f {
                    continue;
                }
                let (Some(k), Some(n)) = (out.last_mut(), newest.last_mut()) else { continue };
                if c >= *n {
                    *n = c;
                    k.expires_ms = exp.filter(|e| *e > 0).map(|e| (created_s + e) * 1000);
                }
            }
            _ => {}
        }
    }
    for k in &mut out {
        k.expired = k.expires_ms.is_some_and(|e| e <= now_ms);
    }
    Ok(out)
}

fn crc24(data: &[u8]) -> u32 {
    let mut crc: u32 = 0x00B7_04CE;
    for b in data {
        crc ^= u32::from(*b) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= 0x0186_4CFB;
            }
        }
    }
    crc & 0x00FF_FFFF
}

/// The keyring as rynas writes it: the accepted packets (trust packets dropped), armored.
pub fn armor(bin: &[u8]) -> Result<String, String> {
    use base64::Engine;
    let ps = packets(bin)?;
    let clean: Vec<u8> = ps.iter().filter(|p| p.tag != 12).flat_map(|p| p.raw.iter().copied()).collect();
    let b64 = base64::engine::general_purpose::STANDARD.encode(&clean);
    let mut out = format!("{BEGIN}\n\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    let c = crc24(&clean).to_be_bytes();
    out.push('=');
    out.push_str(&base64::engine::general_purpose::STANDARD.encode(&c[1..]));
    out.push('\n');
    out.push_str(END);
    out.push('\n');
    Ok(out)
}

/// Dearmor, describe and armor again: what the preview answers and the add checks.
pub fn preview(input: &[u8], now_ms: u64) -> Result<(Vec<KeyInfo>, String), String> {
    let bin = dearmor(input)?;
    let keys = describe(&bin, now_ms)?;
    Ok((keys, armor(&bin)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS: &[u8] = include_bytes!("../../../rynas-tailscale/testdata/tailscale-archive-keyring.gpg");

    #[test]
    fn binary_and_armored_keys() {
        let (keys, arm) = preview(TS, 0).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fingerprint, rynas_tailscale::install::KEY_FINGERPRINT);
        assert!(!keys[0].uids.is_empty(), "{keys:?}");
        assert!(keys[0].created_ms.is_some());
        assert!(arm.starts_with(BEGIN) && arm.ends_with("-----END PGP PUBLIC KEY BLOCK-----\n"));
        // the armored text reads back to the same key, with headers and CRC line
        let with_headers = arm.replacen("\n\n", "\nComment: test\n\n", 1);
        let (again, arm2) = preview(with_headers.as_bytes(), 0).unwrap();
        assert_eq!(again, keys);
        assert_eq!(arm2, arm);
        // two blocks: two keys
        let two = format!("{arm}\n{arm}");
        assert_eq!(preview(two.as_bytes(), 0).unwrap().0.len(), 2);
        // expired by the clock given
        if let Some(e) = keys[0].expires_ms {
            assert!(preview(TS, e + 1).unwrap().0[0].expired);
        }
    }

    #[test]
    fn refusals() {
        assert!(preview(b"hello", 0).is_err());
        assert!(preview(b"-----BEGIN PGP PRIVATE KEY BLOCK-----\nabc\n-----END PGP PRIVATE KEY BLOCK-----\n", 0).unwrap_err().contains("private"));
        // a secret key packet (tag 5, new format)
        assert!(describe(&[0xc5, 0x01, 0x04], 0).unwrap_err().contains("private"));
        // a literal data packet (tag 11)
        let mut odd = TS.to_vec();
        odd.extend([0xcb, 0x01, 0x00]);
        assert!(describe(&odd, 0).unwrap_err().contains("type 11"));
        assert!(preview(&vec![b'a'; KEY_MAX_BYTES + 1], 0).is_err());
        assert_eq!(crc24(b""), 0xB704CE);
    }
}
