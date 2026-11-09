use aes::{Aes128, Aes256};
use hmac::Hmac;
use kbkdf::{Counter, Kbkdf, Params};
use sha2::Sha256;

use super::CryptoError;
type HmacSha256 = Hmac<Sha256>;

/// The type of derived keys for SMB2, outputting from kbkdf.
pub type DerivedKey = [u8; 16];
pub type KeyToDerive = [u8; 16];

pub fn kbkdf_hmacsha256(
    key: &KeyToDerive,
    label: &[u8],
    context: &[u8],
) -> Result<DerivedKey, CryptoError> {
    let counter = Counter::<HmacSha256, Aes128>::default();

    let result = counter
        .derive(
            Params::builder(key)
                .with_label(label)
                .with_context(context)
                .build(),
        )
        // Derivation may only fail with invalid data lengths,
        // which is expected to be correct here.
        .expect("Caller must derive with correct parameters!");

    Ok(result.into())
}

/// rynas patch: the same with L = 256, for the keys of the AES-256 ciphers (MS-SMB2 3.1.4.2:
/// a 16-byte key is refused by AES-256, so a session that negotiated it could never start).
pub fn kbkdf_hmacsha256_256(
    key: &KeyToDerive,
    label: &[u8],
    context: &[u8],
) -> Result<[u8; 32], CryptoError> {
    let counter = Counter::<HmacSha256, Aes256>::default();

    let result = counter
        .derive(
            Params::builder(key)
                .with_label(label)
                .with_context(context)
                .build(),
        )
        .expect("Caller must derive with correct parameters!");

    Ok(result.into())
}
