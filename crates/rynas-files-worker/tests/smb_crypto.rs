//! The crypto of the SMB client on `ring` against the reference it replaced (RustCrypto's
//! aes-gcm, the implementation of the vendored crate before) and known answers. The
//! round trips against a real server are in `src/remote/smb.rs` (`RYNAS_TEST_SMBD`).

use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, aead::AeadInOut};
use smb::crypto::{GmacBackend, force_gmac_backend, make_encrypting_algo, make_gmac_signer, make_signing_algo};
use smb::session::MessageSigner;
use smb::transport::{IoVec, IoVecBuf};
use smb::{Command, EncryptionCipher, Error, Header, HeaderFlags, SigningAlgorithmId};

const MESSAGE_SIZES: &[usize] = &[0, 1, 2, 15, 16, 17, 63, 64, 65, 255, 256, 4095, 4096, 4097, 65535, 65536, 65537, 1 << 20, (1 << 20) + 7, 8 << 20, (8 << 20) + 1];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        self.fill(&mut out);
        out
    }

    fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    fn vec(&mut self, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        self.fill(&mut v);
        v
    }
}

/// A seeded generator: a failure repeats. `RYNAS_TEST_SEED` picks another run.
fn rng() -> Rng {
    let seed = std::env::var("RYNAS_TEST_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15);
    eprintln!("seed {seed}");
    Rng(seed)
}

fn header(message_id: u64, from_server: bool, command: Command) -> Header {
    Header {
        credit_charge: 1,
        status: 0,
        command,
        credit_request: 1,
        flags: HeaderFlags::new().with_server_to_redir(from_server).with_signed(true),
        next_command: 0,
        message_id,
        tree_id: Some(1),
        async_id: None,
        session_id: 0x1122_3344,
        signature: 0,
    }
}

/// The nonce written down from MS-SMB2 3.1.4.1, apart from the code under test.
fn spec_nonce(message_id: u64, from_server: bool, cancel: bool) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&message_id.to_le_bytes());
    let role = u32::from(from_server) | (u32::from(cancel) << 1);
    nonce[8..].copy_from_slice(&role.to_le_bytes());
    nonce
}

fn rustcrypto_gmac(key: &[u8; 16], nonce: &[u8; 12], message: &[u8]) -> u128 {
    let gmac = Aes128Gcm::new_from_slice(key).unwrap();
    let tag = gmac.encrypt_inout_detached(nonce.into(), message, (&mut [] as &mut [u8]).into()).unwrap();
    u128::from_le_bytes(tag.into())
}

#[test]
fn gmac_matches_the_old_implementation() {
    let mut rng = rng();
    let mut sizes = MESSAGE_SIZES.to_vec();
    sizes.extend((0..100).map(|_| (rng.next() % 200_000) as usize));
    for size in sizes {
        let key: [u8; 16] = rng.bytes();
        let message = rng.vec(size);
        // both directions, cancel or not, across the whole range of message ids
        for (message_id, from_server, cancel) in [(rng.next() >> 1 | 1, false, false), (rng.next() >> 1 | 1, true, false), (rng.next() >> 1 | 1, false, true), (1, false, false), (u64::MAX - 1, true, true)] {
            let command = if cancel { Command::Cancel } else { Command::Read };
            let want = rustcrypto_gmac(&key, &spec_nonce(message_id, from_server, cancel), &message);
            // however the message is split, as MessageSigner hands over buffers
            let split = if size == 0 { 0 } else { rng.next() as usize % (size + 1) };
            for backend in [GmacBackend::Ring, GmacBackend::RustCrypto] {
                let mut signer = make_gmac_signer(&key, backend);
                signer.start(&header(message_id, from_server, command));
                signer.update(&message[..split]);
                signer.update(&message[split..]);
                assert_eq!(signer.finalize(), want, "{backend:?} size {size} id {message_id} server {from_server} cancel {cancel}");
            }
        }
    }
}

/// The choice of implementation by CPU is overridable for the tests, and what is chosen signs
/// like both.
#[test]
fn gmac_backend_can_be_forced() {
    let key = [9u8; 16];
    let sign = |backend| {
        force_gmac_backend(backend);
        let mut signer = make_signing_algo(SigningAlgorithmId::AesGmac, &key).unwrap();
        force_gmac_backend(None);
        signer.start(&header(5, false, Command::Read));
        signer.update(b"hello");
        signer.finalize()
    };
    let by_cpu = sign(None);
    assert_eq!(by_cpu, sign(Some(GmacBackend::Ring)));
    assert_eq!(by_cpu, sign(Some(GmacBackend::RustCrypto)));
}

/// What the upstream crate's own test holds: a real session logoff request and its signature
/// (AES-128-GMAC). Through `MessageSigner`, the code path of every message.
#[test]
fn gmac_known_message_from_the_wire() {
    const KEY: [u8; 16] = [0xAC, 0x36, 0xE9, 0x54, 0x3C, 0xD8, 0x88, 0xF0, 0xA8, 0x41, 0x23, 0xE4, 0x6B, 0xB2, 0xA0, 0xD7];
    let header_bytes = vec![
        0xfeu8, 0x53, 0x4d, 0x42, 0x40, 0x0, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x0, 0x1, 0x0, 0x18, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x9, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x53, 0x20, 0xc, 0x21, 0x0, 0x0, 0x0, 0x0, 0x76, 0x23, 0x4b, 0x3c, 0x81, 0x2f, 0x51, 0xab, 0x8a, 0x5c, 0xf9, 0xfa, 0x43, 0xd4, 0xeb, 0x28,
    ];
    let mut h = Header {
        credit_charge: 1,
        status: 0,
        command: Command::Logoff,
        credit_request: 1,
        flags: HeaderFlags::new().with_signed(true).with_priority_mask(1),
        next_command: 0,
        message_id: 9,
        tree_id: Some(0),
        async_id: None,
        session_id: 0x210c_2053,
        signature: 0x28eb_d443_faf9_5c8a_ab51_2f81_3c4b_2376,
    };
    let body = vec![0x4u8, 0x0, 0x0, 0x0];
    let data = IoVec::from(vec![IoVecBuf::from(header_bytes), IoVecBuf::from(body)]);
    for backend in [GmacBackend::Ring, GmacBackend::RustCrypto] {
        let mut signer = MessageSigner::new(make_gmac_signer(&KEY, backend));
        signer.verify_signature(&mut h, &data).expect("the message as captured verifies");
    }
}

/// One flipped bit anywhere fails the check, for every algorithm (AES-GMAC on both its
/// implementations, the others on what they were on).
#[test]
fn bad_signature_or_message_is_rejected() {
    let mut rng = rng();
    let cases = [(SigningAlgorithmId::AesGmac, Some(GmacBackend::Ring)), (SigningAlgorithmId::AesGmac, Some(GmacBackend::RustCrypto)), (SigningAlgorithmId::AesCmac, None), (SigningAlgorithmId::HmacSha256, None)];
    for (algo, backend) in cases {
        let key: [u8; 16] = rng.bytes();
        for size in [0usize, 1, 4, 64, 4096, 1 << 20] {
            let body = rng.vec(size);
            let signer = MessageSigner::new(match backend {
                Some(b) => make_gmac_signer(&key, b),
                None => make_signing_algo(algo, &key).unwrap(),
            });
            let mut h = header(41, true, Command::Read);
            let data = |body: Vec<u8>| IoVec::from(vec![IoVecBuf::from(vec![0u8; Header::STRUCT_SIZE]), IoVecBuf::from(body)]);
            let mut sent = data(body.clone());
            signer.clone().sign_message(&mut h, &mut sent).unwrap();
            assert_ne!(h.signature, 0);
            signer.clone().verify_signature(&mut h, &data(body.clone())).expect("the good message verifies");
            // the signature
            let good = h.signature;
            for bit in [0u32, 1, 63, 64, 127] {
                h.signature = good ^ (1u128 << bit);
                assert!(matches!(signer.clone().verify_signature(&mut h, &data(body.clone())), Err(Error::SignatureVerificationFailed)), "{algo:?} signature bit {bit}");
            }
            h.signature = good;
            // the message
            if size > 0 {
                let mut bad = body.clone();
                bad[size / 2] ^= 0x40;
                assert!(matches!(signer.clone().verify_signature(&mut h, &data(bad)), Err(Error::SignatureVerificationFailed)), "{algo:?} body");
            }
            // the header, also what only the nonce of AES-GMAC sees
            for change in [0, 1, 2] {
                let mut other = header(41, true, Command::Read);
                other.signature = good;
                match change {
                    0 => other.message_id = 42,
                    1 => other.flags = HeaderFlags::new().with_server_to_redir(false).with_signed(true),
                    _ => other.command = Command::Cancel,
                }
                assert!(matches!(signer.clone().verify_signature(&mut other, &data(body.clone())), Err(Error::SignatureVerificationFailed)), "{algo:?} header change {change}");
            }
        }
    }
}

fn rustcrypto_gcm_seal(key: &[u8], nonce: &[u8; 12], aad: &[u8], data: &mut [u8]) -> u128 {
    let tag = match key.len() {
        16 => Aes128Gcm::new_from_slice(key).unwrap().encrypt_inout_detached(nonce.into(), aad, data.into()).unwrap(),
        _ => Aes256Gcm::new_from_slice(key).unwrap().encrypt_inout_detached(nonce.into(), aad, data.into()).unwrap(),
    };
    u128::from_le_bytes(tag.into())
}

fn rustcrypto_gcm_open(key: &[u8], nonce: &[u8; 12], aad: &[u8], data: &mut [u8], tag: u128) -> bool {
    let tag = tag.to_le_bytes();
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key).unwrap().decrypt_inout_detached(nonce.into(), aad, data.into(), (&tag).into()).is_ok(),
        _ => Aes256Gcm::new_from_slice(key).unwrap().decrypt_inout_detached(nonce.into(), aad, data.into(), (&tag).into()).is_ok(),
    }
}

fn nonce16(nonce: &[u8; 12]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..12].copy_from_slice(nonce);
    out
}

#[test]
fn gcm_matches_the_old_implementation_both_ways() {
    let mut rng = rng();
    let mut sizes = MESSAGE_SIZES.to_vec();
    sizes.extend((0..60).map(|_| (rng.next() % 200_000) as usize));
    for (cipher, key_len) in [(EncryptionCipher::Aes128Gcm, 16usize), (EncryptionCipher::Aes256Gcm, 32)] {
        for &size in &sizes {
            let key = rng.vec(key_len);
            let nonce: [u8; 12] = rng.bytes();
            // the 32 bytes of the transform header the SMB encryption authenticates
            let aad = rng.vec(32);
            let plain = rng.vec(size);
            let mut algo = make_encrypting_algo(cipher, &key).unwrap();

            // ring seals, RustCrypto opens
            let mut sealed = plain.clone();
            let tag = algo.encrypt(&mut sealed, &aad, &nonce16(&nonce)).unwrap().signature;
            let mut other = plain.clone();
            assert_eq!(tag, rustcrypto_gcm_seal(&key, &nonce, &aad, &mut other), "{cipher:?} tag size {size}");
            assert!(sealed == other, "{cipher:?} ciphertext size {size}");
            let mut opened = sealed.clone();
            assert!(rustcrypto_gcm_open(&key, &nonce, &aad, &mut opened, tag));
            assert!(opened == plain);

            // RustCrypto seals, ring opens
            let mut sealed = plain.clone();
            let tag = rustcrypto_gcm_seal(&key, &nonce, &aad, &mut sealed);
            let mut opened = sealed.clone();
            algo.decrypt(&mut opened, &aad, &nonce16(&nonce), tag).unwrap();
            assert!(opened == plain, "{cipher:?} open size {size}");

            // rejected: the tag, the additional data, the text
            let mut opened = sealed.clone();
            assert!(algo.decrypt(&mut opened, &aad, &nonce16(&nonce), tag ^ 1).is_err());
            let mut wrong_aad = aad.clone();
            wrong_aad[31] ^= 1;
            let mut opened = sealed.clone();
            assert!(algo.decrypt(&mut opened, &wrong_aad, &nonce16(&nonce), tag).is_err());
            if size > 0 {
                let mut opened = sealed.clone();
                opened[size / 2] ^= 1;
                assert!(algo.decrypt(&mut opened, &aad, &nonce16(&nonce), tag).is_err());
            }
        }
    }
}

/// Published AES-GCM vectors (McGrew & Viega, the GCM specification): test cases 1, 2,
/// 13 and 14, the ones with an all-zero 12-byte nonce and no additional data.
#[test]
fn gcm_known_answers() {
    struct Case {
        cipher: EncryptionCipher,
        key_len: usize,
        plain_hex: &'static str,
        cipher_hex: &'static str,
        tag_hex: &'static str,
    }
    let cases = [
        Case { cipher: EncryptionCipher::Aes128Gcm, key_len: 16, plain_hex: "", cipher_hex: "", tag_hex: "58e2fccefa7e3061367f1d57a4e7455a" },
        Case { cipher: EncryptionCipher::Aes128Gcm, key_len: 16, plain_hex: "00000000000000000000000000000000", cipher_hex: "0388dace60b6a392f328c2b971b2fe78", tag_hex: "ab6e47d42cec13bdf53a67b21257bddf" },
        Case { cipher: EncryptionCipher::Aes256Gcm, key_len: 32, plain_hex: "", cipher_hex: "", tag_hex: "530f8afbc74536b9a963b4f1c4cb738b" },
        Case { cipher: EncryptionCipher::Aes256Gcm, key_len: 32, plain_hex: "00000000000000000000000000000000", cipher_hex: "cea7403d4d606b6e074ec5d3baf39d18", tag_hex: "d0d1c8a799996bf0265b98b5d48ab919" },
    ];
    let hex = |s: &str| (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect::<Vec<u8>>();
    for case in cases {
        let key = vec![0u8; case.key_len];
        let mut algo = make_encrypting_algo(case.cipher, &key).unwrap();
        let mut data = hex(case.plain_hex);
        let tag = algo.encrypt(&mut data, &[], &[0u8; 16]).unwrap().signature;
        assert_eq!(data, hex(case.cipher_hex), "{:?}", case.cipher);
        let want: [u8; 16] = hex(case.tag_hex).try_into().unwrap();
        assert_eq!(tag, u128::from_le_bytes(want), "{:?}", case.cipher);
        algo.decrypt(&mut data, &[], &[0u8; 16], tag).unwrap();
        assert_eq!(data, hex(case.plain_hex));
    }
}

/// Throughput of 1 MiB messages, old against new: run it on one core with
/// `taskset -c 2 cargo test --release --test smb_crypto -- --ignored --nocapture bench`.
#[test]
#[ignore = "a benchmark, not a test"]
fn bench_smb_crypto() {
    use std::time::{Duration, Instant};
    fn run(name: &str, mut one: impl FnMut()) {
        let bytes = 1u64 << 20;
        let (mut total, t) = (0u64, Instant::now());
        while t.elapsed() < Duration::from_secs(2) {
            one();
            total += bytes;
        }
        println!("{name:<28} {:>8.0} MB/s", total as f64 / 1e6 / t.elapsed().as_secs_f64());
    }
    #[cfg(target_arch = "x86_64")]
    println!("cpu: aes={} pclmulqdq={} avx={} ssse3={}", std::arch::is_x86_feature_detected!("aes"), std::arch::is_x86_feature_detected!("pclmulqdq"), std::arch::is_x86_feature_detected!("avx"), std::arch::is_x86_feature_detected!("ssse3"));
    let mut rng = rng();
    let message = rng.vec(1 << 20);
    let key16: [u8; 16] = rng.bytes();
    let key32 = rng.vec(32);
    let h = header(77, true, Command::Read);
    let head = [0u8; 64];
    for (name, backend) in [("AES-GMAC ring", GmacBackend::Ring), ("AES-GMAC RustCrypto", GmacBackend::RustCrypto)] {
        let template = make_gmac_signer(&key16, backend);
        run(name, || {
            let mut signer = template.clone_box();
            signer.start(&h);
            signer.update(&head);
            signer.update(&message);
            std::hint::black_box(signer.finalize());
        });
    }
    let nonce = spec_nonce(77, true, false);
    let template = make_signing_algo(SigningAlgorithmId::AesCmac, &key16).unwrap();
    run("AES-CMAC RustCrypto", || {
        let mut signer = template.clone_box();
        signer.start(&h);
        signer.update(&head);
        signer.update(&message);
        std::hint::black_box(signer.finalize());
    });
    for (name, cipher, key) in [("AES-128-GCM", EncryptionCipher::Aes128Gcm, &key16[..]), ("AES-256-GCM", EncryptionCipher::Aes256Gcm, &key32[..])] {
        let mut algo = make_encrypting_algo(cipher, key).unwrap();
        let mut data = message.clone();
        run(&format!("{name} seal ring"), || {
            std::hint::black_box(algo.encrypt(&mut data, &head[..32], &nonce16(&nonce)).unwrap().signature);
        });
        let mut data = message.clone();
        run(&format!("{name} seal RustCrypto"), || {
            std::hint::black_box(rustcrypto_gcm_seal(key, &nonce, &head[..32], &mut data));
        });
        let mut sealed = message.clone();
        let tag = algo.encrypt(&mut sealed, &head[..32], &nonce16(&nonce)).unwrap().signature;
        let mut data = sealed.clone();
        run(&format!("{name} open ring"), || {
            data.copy_from_slice(&sealed);
            algo.decrypt(&mut data, &head[..32], &nonce16(&nonce), tag).unwrap();
        });
        let mut data = sealed.clone();
        run(&format!("{name} open RustCrypto"), || {
            data.copy_from_slice(&sealed);
            assert!(rustcrypto_gcm_open(key, &nonce, &head[..32], &mut data, tag));
        });
    }
}
