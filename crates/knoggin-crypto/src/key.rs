// SPDX-License-Identifier: AGPL-3.0-only
//! Data keys, wrapped keys and the `KeyProvider` interface.

use std::fmt;

use async_trait::async_trait;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knoggin_core::StoreId;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Length of every key in this crate: data keys, master keys, snapshot keys.
pub const KEY_LEN: usize = 32;

/// One store's 256-bit data key. Wiped on drop; `Debug` prints no bytes.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct DataKey([u8; KEY_LEN]);

impl DataKey {
    /// Wraps raw key bytes, for example the plaintext of a KMS answer.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// 32 new random bytes.
    pub fn random() -> Result<Self, KeyError> {
        let mut key = Self([0; KEY_LEN]);
        fill_random(&mut key.0)?;
        Ok(key)
    }

    /// Decodes base64url without padding (the broker's `open` answer) straight into a key.
    pub fn from_base64url(text: &str) -> Result<Self, KeyError> {
        decode_key(text).map(Self)
    }

    /// The key as base64url without padding, in a buffer that wipes itself.
    pub fn to_base64url(&self) -> Zeroizing<String> {
        encode_key(&self.0)
    }

    /// The key bytes, for the one call that needs them.
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataKey([redacted])")
    }
}

/// A data key wrapped by a master key. Not secret without the master key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedKey {
    /// Names the master key that wrapped it, for example `local:3f9a1c04b2e87d55` or a KMS key ARN.
    pub kek_id: String,
    /// The wrapped bytes; their layout belongs to the provider.
    pub blob: Vec<u8>,
}

/// Errors of key handling.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The master key named by a wrapped key is not available. The store locks with `master_key_missing`.
    #[error("the master key {kek_id} is not in the key set")]
    UnknownMasterKey {
        /// The `kek_id` that was asked for.
        kek_id: String,
    },
    /// The wrapped key did not open: wrong master key, wrong store, or damaged bytes.
    #[error("the data key does not unwrap")]
    Unwrap,
    /// A key set (key file or keychain item) is malformed.
    #[error("invalid key set: {0}")]
    InvalidKeySet(&'static str),
    /// A key in text form is not 32 bytes of base64url.
    #[error("invalid key encoding")]
    Encoding,
    /// The system random source failed.
    #[error("the random source failed")]
    Random,
    /// The key service could not be reached; try again later (KMS).
    #[error("key service unavailable: {0}")]
    Unavailable(String),
}

/// Where the master key lives. Used as `Arc<dyn KeyProvider>`.
#[async_trait]
pub trait KeyProvider: Send + Sync + fmt::Debug {
    /// Makes a new data key for `store` and returns it with its wrapped form.
    async fn generate(&self, store: StoreId) -> Result<(DataKey, WrappedKey), KeyError>;
    /// Unwraps the data key of `store`. A key wrapped for another store does not unwrap.
    async fn unwrap(&self, store: StoreId, wrapped: &WrappedKey) -> Result<DataKey, KeyError>;
}

pub(crate) fn fill_random(out: &mut [u8]) -> Result<(), KeyError> {
    SystemRandom::new().fill(out).map_err(|_| KeyError::Random)
}

pub(crate) fn decode_key(text: &str) -> Result<[u8; KEY_LEN], KeyError> {
    // 43 characters decode to 32 bytes; the spare room lets the decoder report a long input.
    let mut buf = Zeroizing::new([0u8; 48]);
    let n = URL_SAFE_NO_PAD
        .decode_slice(text, &mut buf[..])
        .map_err(|_| KeyError::Encoding)?;
    if n != KEY_LEN {
        return Err(KeyError::Encoding);
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&buf[..KEY_LEN]);
    Ok(key)
}

pub(crate) fn encode_key(key: &[u8; KEY_LEN]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(44));
    URL_SAFE_NO_PAD.encode_string(key, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_prints_no_bytes() {
        let key = DataKey::from_bytes([0xAB; KEY_LEN]);
        let text = format!("{key:?}");
        assert_eq!(text, "DataKey([redacted])");
    }

    #[test]
    fn base64url_round_trip_and_bad_lengths() {
        let key = DataKey::random().unwrap();
        let text = key.to_base64url();
        assert_eq!(text.len(), 43);
        assert_eq!(
            DataKey::from_base64url(&text).unwrap().expose(),
            key.expose()
        );
        assert!(matches!(
            DataKey::from_base64url(&text[..42]),
            Err(KeyError::Encoding)
        ));
        assert!(matches!(
            DataKey::from_base64url(&format!("{}AAAA", *text)),
            Err(KeyError::Encoding)
        ));
        assert!(matches!(
            DataKey::from_base64url("!!!"),
            Err(KeyError::Encoding)
        ));
    }
}
