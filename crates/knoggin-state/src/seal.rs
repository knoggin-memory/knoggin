// SPDX-License-Identifier: AGPL-3.0-only
//! The field key and what is derived from it: sealing personal fields, the HMACs of the
//! index items, and the login cookie's key.
//!
//! A sealed value is `v1.<nonce>.<ciphertext and tag>`, both in base64url without padding,
//! under AES-256-GCM-SIV with a random 12-byte nonce. Under GCM-SIV a repeated nonce shows
//! only whether two values are equal, so keys that are never replaced may seal forever.

use std::fmt;

use aws_lc_rs::aead::{AES_256_GCM_SIV, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knoggin_crypto::DataKey;
use zeroize::Zeroizing;

use crate::db::StateError;

const PREFIX: &str = "v1.";

/// The three keys derived from the field key. `Debug` prints no bytes.
pub struct FieldKeys {
    seal: DataKey,
    index: DataKey,
    login: DataKey,
}

impl FieldKeys {
    /// Derives the seal, index and login keys from the unwrapped field key.
    pub fn new(field_key: &DataKey) -> Self {
        Self {
            seal: field_key.derive(b"knoggin-field-seal-v1"),
            index: field_key.derive(b"knoggin-field-index-v1"),
            login: field_key.derive(b"knoggin-login-v1"),
        }
    }

    /// Seals one field of the item `pk`/`sk`.
    pub(crate) fn seal_field(
        &self,
        pk: &str,
        sk: &str,
        field: &str,
        plain: &str,
    ) -> Result<String, StateError> {
        seal(&self.seal, &field_aad(pk, sk, field), plain.as_bytes())
    }

    /// Opens one sealed field; a value moved to another item or field does not open.
    pub(crate) fn open_field(
        &self,
        pk: &str,
        sk: &str,
        field: &str,
        sealed: &str,
    ) -> Result<String, StateError> {
        let plain = open(&self.seal, &field_aad(pk, sk, field), sealed)?;
        String::from_utf8(plain.to_vec()).map_err(|_| StateError::Corrupt("a sealed field"))
    }

    /// Lower-case hex HMAC-SHA-256 under the index key.
    pub(crate) fn index_hmac(&self, data: &[u8]) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, self.index.expose());
        hex(hmac::sign(&key, data).as_ref())
    }

    /// Seals the login cookie's JSON (associated data `login`).
    pub fn seal_login(&self, plain: &[u8]) -> Result<String, StateError> {
        seal(&self.login, b"login", plain)
    }

    /// Opens a login cookie; anything that does not open is `Corrupt`.
    pub fn open_login(&self, sealed: &str) -> Result<Zeroizing<Vec<u8>>, StateError> {
        open(&self.login, b"login", sealed)
    }
}

impl fmt::Debug for FieldKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FieldKeys([redacted])")
    }
}

fn field_aad(pk: &str, sk: &str, field: &str) -> Vec<u8> {
    format!("{pk}\n{sk}\n{field}").into_bytes()
}

fn cipher(key: &DataKey) -> Result<LessSafeKey, StateError> {
    UnboundKey::new(&AES_256_GCM_SIV, key.expose())
        .map(LessSafeKey::new)
        .map_err(|_| StateError::Internal)
}

fn seal(key: &DataKey, aad: &[u8], plain: &[u8]) -> Result<String, StateError> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| StateError::Internal)?;
    let mut buf = Zeroizing::new(plain.to_vec());
    cipher(key)?
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut *buf,
        )
        .map_err(|_| StateError::Internal)?;
    Ok(format!(
        "{PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(&*buf)
    ))
}

fn open(key: &DataKey, aad: &[u8], sealed: &str) -> Result<Zeroizing<Vec<u8>>, StateError> {
    let bad = || StateError::Corrupt("a sealed value does not open");
    let (nonce, body) = sealed
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.split_once('.'))
        .ok_or_else(bad)?;
    let nonce = URL_SAFE_NO_PAD.decode(nonce).map_err(|_| bad())?;
    let nonce = Nonce::try_assume_unique_for_key(&nonce).map_err(|_| bad())?;
    let mut buf = Zeroizing::new(URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?);
    let len = cipher(key)?
        .open_in_place(nonce, Aad::from(aad), &mut buf)
        .map_err(|_| bad())?
        .len();
    buf.truncate(len);
    Ok(buf)
}

/// Lower-case hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> FieldKeys {
        FieldKeys::new(&DataKey::from_bytes([9; 32]))
    }

    #[test]
    fn a_sealed_field_opens_only_where_it_was_sealed() {
        let k = keys();
        let s = k
            .seal_field("user#a", "-", "email", "x@example.com")
            .unwrap();
        assert!(s.starts_with("v1."));
        assert!(!s.contains("example"));
        assert_eq!(
            k.open_field("user#a", "-", "email", &s).unwrap(),
            "x@example.com"
        );
        // Another record, another sort key, another field.
        assert!(k.open_field("user#b", "-", "email", &s).is_err());
        assert!(k.open_field("user#a", "x", "email", &s).is_err());
        assert!(k.open_field("user#a", "-", "name", &s).is_err());
        // Another field key.
        let other = FieldKeys::new(&DataKey::from_bytes([8; 32]));
        assert!(other.open_field("user#a", "-", "email", &s).is_err());
        // A flipped character, a missing prefix, garbage.
        let mut flipped = s.clone().into_bytes();
        let last = flipped.len() - 1;
        flipped[last] = if flipped[last] == b'A' { b'B' } else { b'A' };
        let flipped = String::from_utf8(flipped).unwrap();
        assert!(k.open_field("user#a", "-", "email", &flipped).is_err());
        assert!(k.open_field("user#a", "-", "email", &s[3..]).is_err());
        assert!(k.open_field("user#a", "-", "email", "v1.x.y").is_err());
    }

    #[test]
    fn two_seals_of_one_value_differ() {
        let k = keys();
        let a = k.seal_field("p", "s", "name", "n").unwrap();
        let b = k.seal_field("p", "s", "name", "n").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn login_cookie_round_trip_and_hmac_is_hex() {
        let k = keys();
        let s = k.seal_login(b"{\"state\":1}").unwrap();
        assert_eq!(&*k.open_login(&s).unwrap(), b"{\"state\":1}");
        let field = k.seal_field("p", "s", "f", "{\"state\":1}").unwrap();
        assert!(k.open_login(&field).is_err());
        let h = k.index_hmac(b"name");
        assert_eq!(h.len(), 64);
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_ne!(
            h,
            FieldKeys::new(&DataKey::from_bytes([8; 32])).index_hmac(b"name")
        );
        assert_eq!(hex(&[0, 0xab, 0x0f]), "00ab0f");
    }
}
