// rynas patch: AES-GCM through ring instead of aes-gcm (RustCrypto), for the same reason as
// AES-GMAC signing (see crypto/signing.rs): every byte of an encrypted share goes through
// GHASH and AES on the worker's one network thread.
use crypto_common::{InvalidLength, typenum};
use ring::aead::{Aad, Algorithm, LessSafeKey, Nonce, Tag, UnboundKey};

use crate::crypto::CryptoError;

use super::*;

#[derive(Clone)]
pub struct AesGcmEncryptor {
    key: LessSafeKey,
}

impl AesGcmEncryptor {
    /// `algorithm` is ring's AES_128_GCM or AES_256_GCM.
    pub fn build(
        algorithm: &'static Algorithm,
        encrypting_key: &[u8],
    ) -> Result<Box<dyn EncryptingAlgo>, CryptoError> {
        // the only failure of UnboundKey::new is a key of the wrong length
        let key = UnboundKey::new(algorithm, encrypting_key)
            .map_err(|_| CryptoError::InvalidLength(InvalidLength))?;
        Ok(Box::new(Self {
            key: LessSafeKey::new(key),
        }))
    }

    /// The 12 bytes of the nonce, the rest of the header's 16 being zero. The caller's
    /// encryption nonces are random, and ring's "assume unique" is exactly that contract.
    fn nonce(&self, nonce: &EncryptionNonce) -> Nonce {
        let nonce: [u8; 12] = trim_nonce::<typenum::U12>(self, nonce).into();
        Nonce::assume_unique_for_key(nonce)
    }
}

impl EncryptingAlgo for AesGcmEncryptor {
    fn encrypt(
        &mut self,
        payload: &mut [u8],
        header_data: &[u8],
        nonce: &EncryptionNonce,
    ) -> Result<EncryptionResult, CryptoError> {
        let tag = self
            .key
            .seal_in_place_separate_tag(self.nonce(nonce), Aad::from(header_data), payload)
            .map_err(|_| aead::Error)?;
        let tag: [u8; 16] = tag.as_ref().try_into().unwrap();
        Ok(EncryptionResult {
            signature: u128::from_le_bytes(tag),
        })
    }

    fn decrypt(
        &mut self,
        payload: &mut [u8],
        header_data: &[u8],
        nonce: &EncryptionNonce,
        signature: u128,
    ) -> Result<(), CryptoError> {
        self.key
            .open_in_place_separate_tag(
                self.nonce(nonce),
                Aad::from(header_data),
                Tag::from(signature.to_le_bytes()),
                payload,
                0..,
            )
            .map_err(|_| aead::Error)?;
        Ok(())
    }

    fn nonce_size(&self) -> usize {
        12
    }

    fn clone_box(&self) -> Box<dyn EncryptingAlgo> {
        Box::new(self.clone())
    }
}

impl std::fmt::Debug for AesGcmEncryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AesGcmEncrypter")
    }
}
