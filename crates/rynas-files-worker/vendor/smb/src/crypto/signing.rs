use smb_msg::*;

use super::CryptoError;

type SigningKey = [u8; 16];

pub fn make_signing_algo(
    signing_algorithm: SigningAlgorithmId,
    signing_key: &SigningKey,
) -> Result<Box<dyn SigningAlgo>, CryptoError> {
    if !SIGNING_ALGOS.contains(&signing_algorithm) {
        return Err(CryptoError::UnsupportedSigningAlgorithm(signing_algorithm));
    }
    if cfg!(feature = "__debug-dump-keys") {
        log::debug!(
            "Using signing algorithm {:?} with key {:02x?}",
            signing_algorithm,
            signing_key
        );
    }
    match signing_algorithm {
        #[cfg(feature = "sign_hmac")]
        SigningAlgorithmId::HmacSha256 => Ok(hmac_signer::HmacSha256Signer::build(signing_key)),
        #[cfg(feature = "sign_cmac")]
        SigningAlgorithmId::AesCmac => Ok(cmac_signer::Cmac128Signer::build(signing_key)?),
        #[cfg(feature = "sign_gmac")]
        SigningAlgorithmId::AesGmac => Ok(gmac_signer::Gmac128Signer::build(signing_key)),
        #[cfg(not(all(feature = "sign_cmac", feature = "sign_gmac", feature = "sign_hmac")))]
        _ => Err(CryptoError::UnsupportedSigningAlgorithm(signing_algorithm)),
    }
}

// rynas patch: AES-GMAC first. A 3.1.1 server picks the first of the client's list it
// supports; GMAC (carry-less multiply) signs several times faster than CMAC, whose AES
// chain is serial, and every message of a session is signed and checked.
pub const SIGNING_ALGOS: &[SigningAlgorithmId] = &[
    #[cfg(feature = "sign_gmac")]
    SigningAlgorithmId::AesGmac,
    #[cfg(feature = "sign_cmac")]
    SigningAlgorithmId::AesCmac,
    #[cfg(feature = "sign_hmac")]
    SigningAlgorithmId::HmacSha256,
];

// rynas patch: the two AES-GMAC implementations, for the tests (see GmacBackend)
#[cfg(feature = "sign_gmac")]
pub use gmac_signer::{GmacBackend, force_gmac_backend};

/// rynas patch: an AES-GMAC signer on one implementation (tests and benchmarks).
#[cfg(feature = "sign_gmac")]
pub fn make_gmac_signer(signing_key: &SigningKey, backend: GmacBackend) -> Box<dyn SigningAlgo> {
    gmac_signer::Gmac128Signer::with_backend(signing_key, backend)
}

/// A trait for SMB signing algorithms.
pub trait SigningAlgo: std::fmt::Debug + Send + Sync {
    /// Start a new signing session. This is called before any data is passed to the signer,
    /// and [SigningAlgo::update] must feed the header data to the signer, in addition to this call.
    ///
    /// An algorithm may implement this function to perform any necessary initialization,
    /// that requires the header data.
    /// This function must be called once per signing session.
    fn start(&mut self, _header: &Header) {
        // Default implementation does nothing.
    }

    /// Update the signing session with new data.
    fn update(&mut self, data: &[u8]);

    /// Finalize the signing session and return the signature.
    ///
    /// This function must be called once per signing session.
    fn finalize(&mut self) -> u128;

    /// Clone the algo into a boxed trait object.
    /// See [EncryptionAlgo::clone_box](super::encryption::EncryptingAlgo::clone_box) for more information.
    fn clone_box(&self) -> Box<dyn SigningAlgo>;
}

#[cfg(feature = "sign_hmac")]
mod hmac_signer {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    use super::*;

    type HmacSha256 = Hmac<Sha256>;

    #[derive(Debug, Clone)]
    pub struct HmacSha256Signer {
        hmac: Option<HmacSha256>,
    }

    impl HmacSha256Signer {
        pub fn build(signing_key: &SigningKey) -> Box<dyn SigningAlgo> {
            Box::new(HmacSha256Signer {
                hmac: Some(HmacSha256::new_from_slice(signing_key).unwrap()),
            })
        }
    }

    impl SigningAlgo for HmacSha256Signer {
        fn update(&mut self, data: &[u8]) {
            self.hmac.as_mut().unwrap().update(data);
        }

        fn finalize(&mut self) -> u128 {
            let result = self.hmac.take().unwrap().finalize().into_bytes();
            u128::from_le_bytes(result[0..16].try_into().unwrap())
        }

        fn clone_box(&self) -> Box<dyn SigningAlgo> {
            Box::new(self.clone())
        }
    }
}

#[cfg(feature = "sign_cmac")]
mod cmac_signer {
    use super::*;
    use aes::Aes128;
    use cmac::{Cmac, KeyInit};
    use hmac::Mac;

    #[derive(Debug, Clone)]
    pub struct Cmac128Signer {
        cmac: Option<Cmac<Aes128>>,
    }

    impl Cmac128Signer {
        pub fn build(signing_key: &SigningKey) -> Result<Box<dyn SigningAlgo>, CryptoError> {
            Ok(Box::new(Cmac128Signer {
                cmac: Some(Cmac::new_from_slice(signing_key)?),
            }))
        }
    }

    impl SigningAlgo for Cmac128Signer {
        fn update(&mut self, data: &[u8]) {
            self.cmac.as_mut().unwrap().update(data);
        }

        fn finalize(&mut self) -> u128 {
            u128::from_le_bytes(self.cmac.take().unwrap().finalize().into_bytes().into())
        }

        fn clone_box(&self) -> Box<dyn SigningAlgo> {
            Box::new(self.clone())
        }
    }
}

#[cfg(feature = "sign_gmac")]
mod gmac_signer {
    use crate::sync_helpers::OnceCell;

    use aes::Aes128;
    use aes_gcm::{
        Aes128Gcm, Key,
        aead::{AeadInOut, KeyInit},
    };
    use binrw::prelude::*;
    use modular_bitfield::prelude::*;
    use ring::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    use std::sync::atomic::{AtomicU8, Ordering};

    use super::*;

    type Gmac128Nonce = [u8; 12];

    /// rynas patch: which AES-GMAC implementation signs. AES-GMAC is the whole of a message
    /// through GHASH, and neither is best everywhere:
    /// - RustCrypto's GHASH (polyval) uses CLMUL only together with AVX. Without AVX
    ///   (Celeron J4125 and other Atom-class NAS CPUs) it runs its software code, and that
    ///   took the worker's one network thread to its limit at 1 GbE.
    /// - ring feeds the additional data one block at a time, with CLMUL also without AVX. It
    ///   is what such a CPU needs, but on a CPU with AVX it is about 3x slower than
    ///   RustCrypto's aggregated GHASH (and GMAC is nothing but additional data).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum GmacBackend {
        Ring,
        RustCrypto,
    }

    // rynas patch: 0 chooses by CPU; for the tests, which must exercise both on any CPU
    static FORCED: AtomicU8 = AtomicU8::new(0);

    /// rynas patch: tests only: use `backend` for the signers made from now on (`None`:
    /// choose by CPU again). Both give the same signatures.
    #[doc(hidden)]
    pub fn force_gmac_backend(backend: Option<GmacBackend>) {
        let value = match backend {
            None => 0,
            Some(GmacBackend::Ring) => 1,
            Some(GmacBackend::RustCrypto) => 2,
        };
        FORCED.store(value, Ordering::Relaxed);
    }

    fn pick_backend() -> GmacBackend {
        match FORCED.load(Ordering::Relaxed) {
            1 => GmacBackend::Ring,
            2 => GmacBackend::RustCrypto,
            _ => by_cpu(),
        }
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    fn by_cpu() -> GmacBackend {
        if std::arch::is_x86_feature_detected!("avx")
            && std::arch::is_x86_feature_detected!("pclmulqdq")
        {
            GmacBackend::RustCrypto
        } else {
            GmacBackend::Ring
        }
    }

    // elsewhere (aarch64: PMULL) RustCrypto has no such gap
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    fn by_cpu() -> GmacBackend {
        GmacBackend::RustCrypto
    }

    #[derive(Clone)]
    enum Engine {
        Ring(LessSafeKey),
        RustCrypto(Aes128Gcm),
    }

    #[derive(Clone)]
    pub struct Gmac128Signer {
        gmac: Engine,
        nonce: OnceCell<Gmac128Nonce>,
        // neither implementation has an online mode: the message is buffered until finalize().
        buffer: Vec<u8>,
    }

    #[allow(dead_code)]
    mod gmac_nonce {
        use super::*;
        #[bitfield]
        pub struct NonceSuffixFlags {
            #[skip(getters)]
            pub msg_id: B64,
            #[skip(getters)]
            pub is_server: bool,
            #[skip(getters)]
            pub is_cancel: bool,
            #[skip]
            __: B30,
        }
    }

    impl Gmac128Signer {
        pub fn build(key: &SigningKey) -> Box<dyn SigningAlgo> {
            Self::with_backend(key, pick_backend())
        }

        /// rynas patch: one backend explicitly (see [`GmacBackend`]).
        pub fn with_backend(key: &SigningKey, backend: GmacBackend) -> Box<dyn SigningAlgo> {
            let gmac = match backend {
                // the only failure of UnboundKey::new is a key of the wrong length
                GmacBackend::Ring => Engine::Ring(LessSafeKey::new(
                    UnboundKey::new(&AES_128_GCM, key).expect("AES-128 key"),
                )),
                GmacBackend::RustCrypto => {
                    Engine::RustCrypto(Aes128Gcm::new(&Key::<Aes128>::from(*key)))
                }
            };
            Box::new(Gmac128Signer {
                gmac,
                nonce: OnceCell::new(),
                buffer: vec![],
            })
        }

        fn make_nonce(header: &Header) -> Gmac128Nonce {
            debug_assert!(header.message_id > 0 && header.message_id != u64::MAX);

            gmac_nonce::NonceSuffixFlags::new()
                .with_msg_id(header.message_id)
                .with_is_cancel(header.command == Command::Cancel)
                .with_is_server(header.flags.server_to_redir())
                .into_bytes()
        }
    }

    impl super::SigningAlgo for Gmac128Signer {
        fn start(&mut self, header: &Header) {
            // The nonce is derived from the message ID.
            self.nonce.set(Self::make_nonce(header)).unwrap();
        }

        fn update(&mut self, data: &[u8]) {
            debug_assert!(self.nonce.get().is_some());

            // Currently buffered until finalized.
            self.buffer.extend_from_slice(data);
        }

        fn finalize(&mut self) -> u128 {
            debug_assert!(self.nonce.get().is_some());

            // rynas patch: GMAC is GCM with the message as the additional data and nothing
            // to encrypt. MS-SMB2 3.1.4.1 / RFC 4543; the nonce is make_nonce's, unchanged.
            let nonce = self.nonce.get().unwrap();
            match &self.gmac {
                Engine::Ring(key) => {
                    let tag = key
                        .seal_in_place_separate_tag(
                            Nonce::assume_unique_for_key(*nonce),
                            Aad::from(&self.buffer),
                            &mut [],
                        )
                        .unwrap();
                    let tag: [u8; 16] = tag.as_ref().try_into().unwrap();
                    u128::from_le_bytes(tag)
                }
                Engine::RustCrypto(gmac) => {
                    let mut empty_data: Vec<u8> = vec![];
                    let result = gmac
                        .encrypt_inout_detached(
                            nonce.into(),
                            &self.buffer,
                            empty_data.as_mut_slice().into(),
                        )
                        .unwrap();
                    u128::from_le_bytes(result.into())
                }
            }
        }

        fn clone_box(&self) -> Box<dyn SigningAlgo> {
            Box::new(self.clone())
        }
    }

    impl std::fmt::Debug for Gmac128Signer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Gmac128Signer").finish()
        }
    }
}
