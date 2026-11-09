//! The crypto of the SMB client on `ring` against the reference it replaced (RustCrypto's
//! aes-gcm, the implementation of the vendored crate before) and known answers. The
//! round trips against a real server are in `src/remote/smb.rs` (`RYNAS_TEST_SMBD`).

use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, aead::AeadInOut};
use smb::connection::transformer::TransformPhase;
use smb::connection::worker::{Worker, WorkerImpl};
use smb::crypto::{ENCRYPTING_ALGOS, GmacBackend, SIGNING_ALGOS, force_gmac_backend, kbkdf_hmacsha256, kbkdf_hmacsha256_256, make_encrypting_algo, make_gmac_signer, make_signing_algo};
use smb::msg_handler::{OutgoingMessage, ReceiveOptions};
use smb::session::MessageSigner;
use smb::transport::{IoVec, IoVecBuf, SmbTransport, SmbTransportRead, SmbTransportWrite, TransportError};
use smb::{Command, EncryptedHeader, EncryptionCipher, Error, Header, HeaderFlags, LogoffRequest, SigningAlgorithmId};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

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

/// The choice of implementation by CPU is overridable for the tests, each signer says which
/// one it runs on, and what is chosen signs like both.
#[test]
fn gmac_backend_can_be_forced() {
    // what the CPU decides, written down apart from the code under test
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    let by_cpu = if std::arch::is_x86_feature_detected!("avx") && std::arch::is_x86_feature_detected!("pclmulqdq") { GmacBackend::RustCrypto } else { GmacBackend::Ring };
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    let by_cpu = GmacBackend::RustCrypto;

    let key = [9u8; 16];
    let sign = |backend| {
        force_gmac_backend(backend);
        let mut signer = make_signing_algo(SigningAlgorithmId::AesGmac, &key).unwrap();
        force_gmac_backend(None);
        let ran_on = signer.gmac_backend().expect("an AES-GMAC signer");
        signer.start(&header(5, false, Command::Read));
        signer.update(b"hello");
        (signer.finalize(), ran_on)
    };
    let (chosen, ran_on) = sign(None);
    assert_eq!(ran_on, by_cpu, "the implementation the CPU calls for");
    for backend in [GmacBackend::Ring, GmacBackend::RustCrypto] {
        let (signature, ran_on) = sign(Some(backend));
        assert_eq!(ran_on, backend, "forced to {backend:?}");
        assert_eq!(signature, chosen, "{backend:?} signs differently");
    }
    assert_eq!(make_signing_algo(SigningAlgorithmId::AesCmac, &key).unwrap().gmac_backend(), None);
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

/// A worked SP 800-108 counter-mode derivation, computed apart from this crate (Python's
/// hmac/hashlib): one HMAC-SHA256 block of `00000001 || label || 00 || context || L`, with
/// the session key, label and preauth hash of the upstream crate's own key test. The
/// 128-bit answer is that test's signing key: the input is the same, only L differs, and L
/// goes into the hash (the first 16 bytes do not match).
#[test]
fn kbkdf_known_answers() {
    const SESSION_KEY: [u8; 16] = [0xDA, 0x90, 0xB1, 0xDF, 0x80, 0x5C, 0x34, 0x9F, 0x88, 0x86, 0xBA, 0x02, 0x9E, 0xA4, 0x5C, 0xB6];
    const PREAUTH_HASH: [u8; 64] = [
        0x47, 0x95, 0x78, 0xb1, 0x87, 0x23, 0x05, 0x6a, 0x4c, 0x3e, 0x6f, 0x73, 0x2f, 0x36, 0xf1, 0x9c, 0xcc, 0xdd, 0x51, 0x6f, 0x49, 0x56, 0x6b, 0xa0, 0x43, 0xce, 0x59, 0x6a, 0x13, 0x42, 0x27, 0xd9, 0x64, 0xef, 0x0a, 0xa6, 0xa6, 0x27, 0x1a, 0xfe, 0x4f, 0xe6, 0x4b, 0x4d, 0x8c, 0xb2, 0xe6, 0xa1, 0x95, 0x11, 0xed, 0xbb, 0xf6, 0xd7, 0x7d, 0xce, 0xf0, 0x33, 0xda, 0xed, 0x8c, 0x71, 0x81, 0xb2,
    ];
    let label = b"SMBSigningKey\x00";
    let key128 = kbkdf_hmacsha256(&SESSION_KEY, label, &PREAUTH_HASH).unwrap();
    assert_eq!(key128, [0x6D, 0xAC, 0xCE, 0xDE, 0x5B, 0x4E, 0x36, 0x08, 0xAD, 0x6E, 0xA5, 0x47, 0x33, 0xCA, 0x31, 0x63]);
    let key256 = kbkdf_hmacsha256_256(&SESSION_KEY, label, &PREAUTH_HASH).unwrap();
    let want: [u8; 32] = [0xa9, 0x1a, 0xac, 0xd6, 0x65, 0x74, 0xf5, 0x40, 0x18, 0xdb, 0x03, 0x26, 0x91, 0xaa, 0xc8, 0x70, 0xe2, 0x82, 0xa5, 0xd0, 0xba, 0x17, 0x98, 0xfe, 0x10, 0xf8, 0xa6, 0x6b, 0x5a, 0xd5, 0xf3, 0xe5];
    assert_eq!(key256, want);
    assert_ne!(key128, key256[..16]);
}

/// The additional data of the AEAD: MS-SMB2 2.2.41, the transform header from the nonce on.
#[test]
fn aead_bytes_are_the_transform_header_after_the_signature() {
    let nonce: [u8; 16] = std::array::from_fn(|i| i as u8 + 1);
    let header = EncryptedHeader { signature: 0xFFFF_FFFF, nonce, original_message_size: 0x0102_0304, session_id: 0x1122_3344_5566_7788 };
    let mut want = Vec::new();
    want.extend_from_slice(&nonce);
    want.extend_from_slice(&[0x04, 0x03, 0x02, 0x01]); // OriginalMessageSize
    want.extend_from_slice(&[0, 0]); // Reserved
    want.extend_from_slice(&[1, 0]); // Flags: encrypted
    want.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]); // SessionId
    assert_eq!(header.aead_bytes().as_slice(), want.as_slice());
    assert_eq!(want.len(), 32);
}

/// What the client offers first is what a 3.1.1 server picks.
#[test]
fn preference_order_of_the_algorithms() {
    assert_eq!(ENCRYPTING_ALGOS, [EncryptionCipher::Aes128Gcm, EncryptionCipher::Aes256Gcm, EncryptionCipher::Aes128Ccm, EncryptionCipher::Aes256Ccm]);
    assert_eq!(SIGNING_ALGOS, [SigningAlgorithmId::AesGmac, SigningAlgorithmId::AesCmac, SigningAlgorithmId::HmacSha256]);
}

const TEST_SESSION: u64 = 0x5500_0000_1234;
const TEST_KEY: [u8; 16] = [0x42; 16];

/// A transport that hands the worker the frames the test sends down `frames`, and answers
/// "nothing yet" otherwise, as a socket with a read timeout does.
struct FakeTransport {
    frames: mpsc::Receiver<Vec<u8>>,
}

struct NoWrites;

impl SmbTransportRead for FakeTransport {
    fn receive_exact(&mut self, _out: &mut [u8]) -> Result<(), TransportError> {
        unreachable!("the test's frames come whole, through receive")
    }

    fn receive(&mut self) -> Result<Vec<u8>, TransportError> {
        match self.frames.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => Ok(frame),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(TransportError::IoError(std::io::ErrorKind::WouldBlock.into())),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(TransportError::NotConnected),
        }
    }

    fn set_read_timeout(&self, _timeout: Duration) -> Result<(), TransportError> {
        Ok(())
    }
}

impl SmbTransportWrite for NoWrites {
    fn send_raw(&mut self, _buf: &[u8]) -> Result<(), TransportError> {
        Ok(())
    }
}

/// A write that finds the connection gone, which ends the worker's send thread. Dropped when
/// that thread returns, after its receiving end: the message on `gone` says it is over.
struct BrokenWrites {
    gone: mpsc::Sender<()>,
}

impl SmbTransportWrite for BrokenWrites {
    fn send_raw(&mut self, _buf: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::NotConnected)
    }
}

impl Drop for BrokenWrites {
    fn drop(&mut self) {
        let _ = self.gone.send(());
    }
}

/// [`FakeTransport`] and the write half before they are split.
struct FakeConnection(FakeTransport, Box<dyn SmbTransportWrite>);

impl SmbTransportRead for FakeConnection {
    fn receive_exact(&mut self, out: &mut [u8]) -> Result<(), TransportError> {
        self.0.receive_exact(out)
    }

    fn set_read_timeout(&self, timeout: Duration) -> Result<(), TransportError> {
        self.0.set_read_timeout(timeout)
    }
}

impl SmbTransportWrite for FakeConnection {
    fn send_raw(&mut self, _buf: &[u8]) -> Result<(), TransportError> {
        Ok(())
    }
}

impl SmbTransport for FakeConnection {
    fn connect(&mut self, _server_name: &str, _address: SocketAddr) -> Result<(), TransportError> {
        Ok(())
    }

    fn default_port(&self) -> u16 {
        445
    }

    fn split(self: Box<Self>) -> Result<(Box<dyn SmbTransportRead>, Box<dyn SmbTransportWrite>), TransportError> {
        Ok((Box::new(self.0), self.1))
    }

    fn remote_address(&self) -> Result<SocketAddr, TransportError> {
        Ok(SocketAddr::from(([127, 0, 0, 1], 445)))
    }
}

/// A plain LOGOFF response (MS-SMB2 2.2.1, 2.2.8), written out by hand.
fn plain_logoff_response(message_id: u64, session_id: u64) -> Vec<u8> {
    let mut frame = vec![0xFE, b'S', b'M', b'B', 64, 0, 1, 0];
    frame.extend_from_slice(&0u32.to_le_bytes()); // status
    frame.extend_from_slice(&2u16.to_le_bytes()); // LOGOFF
    frame.extend_from_slice(&1u16.to_le_bytes()); // credits
    frame.extend_from_slice(&1u32.to_le_bytes()); // flags: server to client
    frame.extend_from_slice(&0u32.to_le_bytes()); // next command
    frame.extend_from_slice(&message_id.to_le_bytes());
    frame.extend_from_slice(&0u32.to_le_bytes()); // reserved
    frame.extend_from_slice(&1u32.to_le_bytes()); // tree id
    frame.extend_from_slice(&session_id.to_le_bytes());
    frame.extend_from_slice(&[0u8; 16]); // signature
    frame.extend_from_slice(&[4, 0, 0, 0]);
    frame
}

/// An encrypted frame as a server sends it: transform header, then the sealed message.
fn encrypted_frame(key: &[u8], message: &[u8], session_id: u64) -> Vec<u8> {
    let mut nonce = [0u8; 16];
    nonce[..12].copy_from_slice(&[7, 7, 7, 7, 1, 2, 3, 4, 5, 6, 7, 8]);
    let mut aad = Vec::with_capacity(32);
    aad.extend_from_slice(&nonce);
    aad.extend_from_slice(&(message.len() as u32).to_le_bytes());
    aad.extend_from_slice(&[0, 0, 1, 0]); // reserved, flags
    aad.extend_from_slice(&session_id.to_le_bytes());
    let mut sealed = message.to_vec();
    let tag = make_encrypting_algo(EncryptionCipher::Aes128Gcm, key).unwrap().encrypt(&mut sealed, &aad, &nonce).unwrap().signature;
    let mut frame = vec![0xFD, b'S', b'M', b'B'];
    frame.extend_from_slice(&tag.to_le_bytes());
    frame.extend_from_slice(&aad);
    frame.extend_from_slice(&sealed);
    frame
}

/// A worker on a [`FakeTransport`] with one encrypted session, and the sender of its frames.
fn worker_with_encrypted_session() -> (Arc<WorkerImpl>, mpsc::Sender<Vec<u8>>) {
    let (frames_tx, frames) = mpsc::channel();
    let worker = <WorkerImpl as Worker>::start(Box::new(FakeConnection(FakeTransport { frames }, Box::new(NoWrites))), Duration::from_secs(30)).unwrap();
    worker.transformer().add_session_for_test(TEST_SESSION, EncryptionCipher::Aes128Gcm, &TEST_KEY).unwrap();
    (worker, frames_tx)
}

fn wait_for(worker: &Arc<WorkerImpl>, message_id: u64) -> std::thread::JoinHandle<smb::Result<smb::msg_handler::IncomingMessage>> {
    let worker = Arc::clone(worker);
    std::thread::spawn(move || worker.receive_next(&ReceiveOptions { msg_id: message_id, ..Default::default() }))
}

fn assert_authentication_failure<T: std::fmt::Debug>(res: smb::Result<T>, what: &str) {
    match res {
        Err(Error::TranformFailed(e)) => {
            assert!(matches!(e.phase, TransformPhase::EncryptDecrypt) && !e.outgoing && e.msg_id.is_none(), "{what}: {e}");
            assert!(e.why.contains("failed authentication"), "{what}: {e}");
        }
        other => panic!("{what}: not an authentication failure: {other:?}"),
    }
}

/// Stopping after the send thread has already exited (a write found the connection gone) is
/// a normal close: with `panic = "abort"` a panic here would end the worker and its jobs.
#[test]
fn stop_after_the_send_thread_has_exited() {
    let (_frames_tx, frames) = mpsc::channel();
    let (gone_tx, gone) = mpsc::channel();
    let worker = <WorkerImpl as Worker>::start(Box::new(FakeConnection(FakeTransport { frames }, Box::new(BrokenWrites { gone: gone_tx }))), Duration::from_secs(30)).unwrap();
    worker.send(OutgoingMessage::new(LogoffRequest {}.into())).unwrap();
    gone.recv_timeout(Duration::from_secs(5)).expect("the send thread ended");
    worker.stop().expect("a clean stop");
}

/// A tampered encrypted frame ends the connection at once, long before the watchdog (30 s
/// here), for every waiter and for what comes later; nothing of it is delivered, and it is
/// not progress. Before it: a good frame is delivered and is progress; a frame that does not
/// verify is neither.
#[test]
fn tampered_encrypted_frame_fails_the_connection_at_once() {
    let settle = || std::thread::sleep(Duration::from_millis(30));
    type Change = fn(&mut Vec<u8>);
    let changes: [(&str, Change); 5] = [
        ("the ciphertext", |f| *f.last_mut().unwrap() ^= 1),
        ("the tag", |f| f[4] ^= 1),
        ("the nonce", |f| f[20] ^= 1),
        ("the session id", |f| f[44] ^= 1),
        ("the flags", |f| f[42] ^= 1),
    ];
    for (name, tamper) in changes {
        let (worker, frames) = worker_with_encrypted_session();
        let good = encrypted_frame(&TEST_KEY, &plain_logoff_response(7, TEST_SESSION), TEST_SESSION);

        settle();
        let before = worker.activity().load(Ordering::Relaxed);
        let waiter = wait_for(&worker, 7);
        frames.send(good.clone()).unwrap();
        let msg = waiter.join().unwrap().expect("the good frame arrives");
        assert!(msg.form.encrypted, "{name}");
        let progress = worker.activity().load(Ordering::Relaxed);
        assert!(progress > before, "a verified frame is progress ({name})");

        // no signature on a session that needs one: accepted by the transformer, not progress
        settle();
        frames.send(plain_logoff_response(20, TEST_SESSION)).unwrap();
        // a signed frame that cannot verify, with a message id: its own error, not fatal, not progress
        let mut signed = plain_logoff_response(21, TEST_SESSION);
        signed[16] |= 8; // SMB2_FLAGS_SIGNED
        signed[48] ^= 1;
        frames.send(signed).unwrap();
        settle();
        assert!(!worker.stopped(), "{name}");
        assert!(matches!(wait_for(&worker, 21).join().unwrap(), Err(Error::TranformFailed(_))), "{name}");
        assert_eq!(worker.activity().load(Ordering::Relaxed), progress, "an unverified frame is not progress ({name})");

        // two waiters, then the tampered frame; the first must not get the 7's copy
        let (first, second) = (wait_for(&worker, 11), wait_for(&worker, 12));
        settle();
        let mut bad = encrypted_frame(&TEST_KEY, &plain_logoff_response(11, TEST_SESSION), TEST_SESSION);
        tamper(&mut bad);
        let start = Instant::now();
        frames.send(bad).unwrap();
        assert_authentication_failure(first.join().unwrap(), name);
        assert_authentication_failure(second.join().unwrap(), name);
        assert!(start.elapsed() < Duration::from_secs(5), "{name}: took {:?}", start.elapsed());
        assert_eq!(worker.activity().load(Ordering::Relaxed), progress, "a tampered frame is not progress ({name})");
        assert!(worker.stopped(), "{name}");

        // and later requests are told the same, at once
        let start = Instant::now();
        assert_authentication_failure(wait_for(&worker, 13).join().unwrap(), name);
        assert!(start.elapsed() < Duration::from_secs(5), "{name}: took {:?}", start.elapsed());
        worker.stop().unwrap();
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
