// SPDX-License-Identifier: AGPL-3.0-only
//! API keys, random tokens, their hashes and constant-time comparison.

use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::digest::{SHA256, digest};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zeroize::Zeroizing;

use crate::key::{KeyError, fill_random};

/// What every API key starts with.
pub const API_KEY_PREFIX: &str = "knoggin_live_";

/// Length of the visible prefix of an API key, taken from its random part.
pub const VISIBLE_PREFIX_LEN: usize = 8;

/// A new API key: the secret (shown once), its SHA-256 (stored), and its visible prefix.
pub fn new_api_key() -> Result<(String, [u8; 32], String), KeyError> {
    let random = random_token()?;
    let prefix = random[..VISIBLE_PREFIX_LEN].to_string();
    let secret = format!("{API_KEY_PREFIX}{random}");
    let hash = hash_secret(&secret);
    Ok((secret, hash, prefix))
}

/// SHA-256 of the whole secret string. A fast hash is right: the secrets are 256 random bits.
pub fn hash_secret(secret: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, secret.as_bytes()).as_ref());
    out
}

/// 32 random bytes in base64url without padding (43 characters).
pub fn random_token() -> Result<String, KeyError> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    fill_random(&mut bytes[..])?;
    Ok(URL_SAFE_NO_PAD.encode(&bytes[..]))
}

/// Constant-time equality, for tokens, key hashes and MACs.
pub fn eq(a: &[u8], b: &[u8]) -> bool {
    verify_slices_are_equal(a, b).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_base64url(s: &str) -> bool {
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }

    #[test]
    fn api_key_format_prefix_and_hash() {
        let (secret, hash, prefix) = new_api_key().unwrap();
        let random = secret.strip_prefix("knoggin_live_").unwrap();
        assert_eq!(random.len(), 43);
        assert!(is_base64url(random));
        assert_eq!(prefix, &random[..8]);
        assert_eq!(hash, hash_secret(&secret));
        let (other, other_hash, _) = new_api_key().unwrap();
        assert_ne!(other, secret);
        assert_ne!(other_hash, hash);
    }

    #[test]
    fn hash_is_sha256_of_the_whole_string() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(hash_secret("abc"), expected);
    }

    #[test]
    fn random_tokens() {
        let a = random_token().unwrap();
        let b = random_token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(is_base64url(&a));
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq() {
        assert!(eq(b"same", b"same"));
        assert!(eq(b"", b""));
        assert!(!eq(b"same", b"sama"));
        assert!(!eq(b"same", b"same "));
        assert!(!eq(b"", b"x"));
    }
}
