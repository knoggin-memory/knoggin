// SPDX-License-Identifier: AGPL-3.0-only
//! Data keys, wrapped keys and the `KeyProvider` interface.

use std::fmt;

use async_trait::async_trait;
use aws_lc_rs::hkdf::{HKDF_SHA256, Salt};
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

    /// A 32-byte key derived from this one: HKDF-SHA-256 with an empty salt and `info`.
    /// Gives the field-seal, field-index and login keys, and the Postgres state key.
    #[expect(
        clippy::expect_used,
        reason = "HKDF-SHA-256 refuses only outputs over 255 * 32 bytes; this one is 32"
    )]
    pub fn derive(&self, info: &[u8]) -> Self {
        let mut out = Self([0; KEY_LEN]);
        Salt::new(HKDF_SHA256, &[])
            .extract(&self.0)
            .expand(&[info], HKDF_SHA256)
            .and_then(|okm| okm.fill(&mut out.0))
            .expect("a 32-byte HKDF-SHA-256 output");
        out
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
    /// The wrapped key did not open: wrong master key, wrong store or generation, or damaged
    /// bytes.
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

/// What a wrapped data key is bound to: one store and one generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyScope {
    /// The store the key belongs to.
    pub store: StoreId,
    /// The generation the key belongs to.
    pub generation: u64,
}

impl KeyScope {
    /// The field key and the local state file's key: the nil store, generation 0.
    pub const STATE: KeyScope = KeyScope {
        store: StoreId::from_bytes([0; 16]),
        generation: 0,
    };
    /// The key that seals state-database items: store `00000000-0000-0000-0000-000000000001`,
    /// generation 0.
    pub const ITEMS: KeyScope = KeyScope {
        store: StoreId::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        generation: 0,
    };
}

/// Where the master key lives. Used as `Arc<dyn KeyProvider>`.
#[async_trait]
pub trait KeyProvider: Send + Sync + fmt::Debug {
    /// Makes a new data key for `scope` and returns it with its wrapped form.
    async fn generate(&self, scope: KeyScope) -> Result<(DataKey, WrappedKey), KeyError>;
    /// Unwraps a data key wrapped for `scope`. A key wrapped for another store or another
    /// generation does not unwrap.
    async fn unwrap(&self, scope: KeyScope, wrapped: &WrappedKey) -> Result<DataKey, KeyError>;
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
    fn derive_is_hkdf_sha256_with_an_empty_salt() {
        use aws_lc_rs::hmac::{HMAC_SHA256, Key, sign};
        let ikm = DataKey::from_bytes([7; KEY_LEN]);
        // RFC 5869 by hand: PRK = HMAC(zero salt, IKM), T(1) = HMAC(PRK, info || 0x01).
        let prk = sign(&Key::new(HMAC_SHA256, &[0; 32]), ikm.expose());
        let t1 = sign(
            &Key::new(HMAC_SHA256, prk.as_ref()),
            b"knoggin-field-seal-v1\x01",
        );
        let derived = ikm.derive(b"knoggin-field-seal-v1");
        assert_eq!(derived.expose().as_slice(), t1.as_ref());
        assert_ne!(
            ikm.derive(b"knoggin-field-index-v1").expose(),
            derived.expose()
        );
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
