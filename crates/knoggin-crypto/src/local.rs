// SPDX-License-Identifier: AGPL-3.0-only
//! The master key set and `LocalKeyProvider`, which wraps data keys with it.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};

use async_trait::async_trait;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use aws_lc_rs::digest::{SHA256, digest};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use knoggin_core::StoreId;
use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::key::{
    DataKey, KEY_LEN, KeyError, KeyProvider, WrappedKey, decode_key, encode_key, fill_random,
};

/// Prefix of the `kek_id` of every key in a local key set.
pub const LOCAL_KEK_PREFIX: &str = "local:";

const ID_INFO: &[u8] = b"knoggin-kek-id-v1";
const WRAP_AAD: &[u8] = b"knoggin-dek-wrap-v1";
const TAG_LEN: usize = 16;
/// Length of a locally wrapped data key: nonce, ciphertext, tag.
pub const WRAPPED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

#[derive(Zeroize, ZeroizeOnDrop)]
struct MasterKey([u8; KEY_LEN]);

impl<'de> Deserialize<'de> for MasterKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Decodes from the parser's own buffer, so no `String` copy of the key text is made.
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = MasterKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("32 bytes in base64url")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<MasterKey, E> {
                decode_key(s)
                    .map(MasterKey)
                    .map_err(|_| E::custom("a key is not 32 bytes of base64url"))
            }
        }
        d.deserialize_str(V)
    }
}

/// A master key set: the content of a key file, or of a keychain item in base64.
///
/// `{ "version": 1, "current": "<id>", "keys": { "<id>": "<32 bytes, base64url>" } }`.
/// Retired keys stay in the set so that old snapshot headers still open.
pub struct MasterKeySet {
    current: String,
    keys: BTreeMap<String, MasterKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeySetJson {
    version: u32,
    current: String,
    keys: BTreeMap<String, MasterKey>,
}

impl MasterKeySet {
    /// A new set with one random key as `current`.
    pub fn generate() -> Result<Self, KeyError> {
        let mut set = Self {
            current: String::new(),
            keys: BTreeMap::new(),
        };
        set.rotate()?;
        Ok(set)
    }

    /// A set of exactly this one key. Fixed keys are for tests only.
    pub(crate) fn from_key(key: [u8; KEY_LEN]) -> Self {
        let id = key_id(&key);
        Self {
            current: id.clone(),
            keys: BTreeMap::from([(id, MasterKey(key))]),
        }
    }

    /// Adds a new random key and makes it `current`. The old keys stay.
    pub fn rotate(&mut self) -> Result<(), KeyError> {
        let mut key = MasterKey([0; KEY_LEN]);
        fill_random(&mut key.0)?;
        let id = key_id(&key.0);
        self.current.clone_from(&id);
        self.keys.insert(id, key);
        Ok(())
    }

    /// Parses the key set JSON. Every key's ID must match its bytes.
    pub fn from_json(json: &[u8]) -> Result<Self, KeyError> {
        let raw: KeySetJson = serde_json::from_slice(json)
            .map_err(|_| KeyError::InvalidKeySet("not key set JSON"))?;
        if raw.version != 1 {
            return Err(KeyError::InvalidKeySet("unknown version"));
        }
        if !raw.keys.contains_key(&raw.current) {
            return Err(KeyError::InvalidKeySet("current names no key in the set"));
        }
        if raw.keys.iter().any(|(id, key)| *id != key_id(&key.0)) {
            return Err(KeyError::InvalidKeySet("a key ID does not match its key"));
        }
        Ok(Self {
            current: raw.current,
            keys: raw.keys,
        })
    }

    /// The key set JSON, in a buffer that wipes itself.
    pub fn to_json(&self) -> Zeroizing<String> {
        // Reserved up front so that no reallocation leaves a copy behind.
        let mut out = Zeroizing::new(String::with_capacity(64 + self.keys.len() * 72));
        let _ = write!(
            out,
            "{{\"version\":1,\"current\":\"{}\",\"keys\":{{",
            self.current
        );
        for (i, (id, key)) in self.keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(out, "\"{id}\":\"{}\"", *encode_key(&key.0));
        }
        out.push_str("}}");
        out
    }

    /// Parses a keychain item: the standard base64 (with padding) of the key set JSON.
    pub fn from_keychain_text(text: &[u8]) -> Result<Self, KeyError> {
        let text = text.trim_ascii();
        let mut json = Zeroizing::new(vec![0u8; text.len().div_ceil(4) * 3]);
        let n = STANDARD
            .decode_slice(text, &mut json[..])
            .map_err(|_| KeyError::InvalidKeySet("keychain item is not base64"))?;
        Self::from_json(&json[..n])
    }

    /// The keychain form: the standard base64 of the key set JSON, with padding.
    pub fn to_keychain_text(&self) -> Zeroizing<String> {
        let json = self.to_json();
        let mut out = Zeroizing::new(String::with_capacity(json.len().div_ceil(3) * 4));
        STANDARD.encode_string(json.as_bytes(), &mut out);
        out
    }

    /// `local:` and the ID of the current key, as recorded in `key-source.json`.
    pub fn current_kek_id(&self) -> String {
        format!("{LOCAL_KEK_PREFIX}{}", self.current)
    }

    /// A key derived from the current key with HKDF-SHA-256 (empty salt, `info`): on one
    /// host, the key that seals every Postgres document.
    pub fn derive_current(&self, info: &[u8]) -> Result<DataKey, KeyError> {
        Ok(DataKey::from_bytes(self.current_key()?.0).derive(info))
    }

    /// Number of keys in the set, the current one included.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Always false: a set holds at least its current key.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn current_key(&self) -> Result<&MasterKey, KeyError> {
        self.keys
            .get(&self.current)
            .ok_or(KeyError::InvalidKeySet("current names no key in the set"))
    }
}

impl fmt::Debug for MasterKeySet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MasterKeySet")
            .field("current", &self.current)
            .field("keys", &self.keys.len())
            .finish()
    }
}

/// First 16 hex characters of `SHA-256("knoggin-kek-id-v1" || key)`.
fn key_id(key: &[u8; KEY_LEN]) -> String {
    let mut input = Zeroizing::new([0u8; ID_INFO.len() + KEY_LEN]);
    input[..ID_INFO.len()].copy_from_slice(ID_INFO);
    input[ID_INFO.len()..].copy_from_slice(key);
    let hash = digest(&SHA256, &input[..]);
    hash.as_ref()[..8].iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn wrap_aad(kek_id: &str, store: StoreId) -> Vec<u8> {
    [WRAP_AAD, kek_id.as_bytes(), store.as_bytes()].concat()
}

fn aead_key(key: &MasterKey) -> Result<LessSafeKey, KeyError> {
    UnboundKey::new(&AES_256_GCM, &key.0)
        .map(LessSafeKey::new)
        .map_err(|_| KeyError::Unwrap)
}

/// Wraps data keys with a master key set that its caller loaded. Reads no file and never reloads.
pub struct LocalKeyProvider {
    keys: MasterKeySet,
}

impl LocalKeyProvider {
    /// Takes the set that the binary loaded from the key file or the keychain.
    pub fn new(keys: MasterKeySet) -> Self {
        Self { keys }
    }

    /// Wraps `key` under the current master key. Used by master-key rotation only.
    pub fn wrap(&self, store: StoreId, key: &DataKey) -> Result<WrappedKey, KeyError> {
        let kek_id = self.keys.current_kek_id();
        let sealing = aead_key(self.keys.current_key()?)?;
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce)?;
        let mut blob = Vec::with_capacity(WRAPPED_LEN);
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(key.expose());
        let tag = sealing
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(wrap_aad(&kek_id, store)),
                &mut blob[NONCE_LEN..],
            )
            .map_err(|_| KeyError::Unwrap)?;
        blob.extend_from_slice(tag.as_ref());
        Ok(WrappedKey { kek_id, blob })
    }

    /// The synchronous body of `KeyProvider::generate`.
    pub(crate) fn generate_key(&self, store: StoreId) -> Result<(DataKey, WrappedKey), KeyError> {
        let key = DataKey::random()?;
        let wrapped = self.wrap(store, &key)?;
        Ok((key, wrapped))
    }

    /// The synchronous body of `KeyProvider::unwrap`.
    pub(crate) fn unwrap_key(
        &self,
        store: StoreId,
        wrapped: &WrappedKey,
    ) -> Result<DataKey, KeyError> {
        let unknown = || KeyError::UnknownMasterKey {
            kek_id: wrapped.kek_id.clone(),
        };
        let id = wrapped
            .kek_id
            .strip_prefix(LOCAL_KEK_PREFIX)
            .ok_or_else(unknown)?;
        let master = self.keys.keys.get(id).ok_or_else(unknown)?;
        if wrapped.blob.len() != WRAPPED_LEN {
            return Err(KeyError::Unwrap);
        }
        let (nonce, sealed) = wrapped.blob.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| KeyError::Unwrap)?;
        let mut buf = Zeroizing::new(sealed.to_vec());
        let plain = aead_key(master)?
            .open_in_place(nonce, Aad::from(wrap_aad(&wrapped.kek_id, store)), &mut buf)
            .map_err(|_| KeyError::Unwrap)?;
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(plain);
        Ok(DataKey::from_bytes(key))
    }
}

impl fmt::Debug for LocalKeyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalKeyProvider")
            .field("keys", &self.keys)
            .finish()
    }
}

#[async_trait]
impl KeyProvider for LocalKeyProvider {
    async fn generate(&self, store: StoreId) -> Result<(DataKey, WrappedKey), KeyError> {
        self.generate_key(store)
    }

    async fn unwrap(&self, store: StoreId, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        self.unwrap_key(store, wrapped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(n: u8) -> StoreId {
        StoreId::from_bytes([n; 16])
    }

    #[tokio::test]
    async fn generate_then_unwrap() {
        let p = LocalKeyProvider::new(MasterKeySet::generate().unwrap());
        let (key, wrapped) = p.generate(store(1)).await.unwrap();
        assert_eq!(wrapped.blob.len(), 60);
        assert_eq!(wrapped.kek_id, p.keys.current_kek_id());
        assert!(wrapped.kek_id.starts_with("local:"));
        assert_eq!(wrapped.kek_id.len(), "local:".len() + 16);
        let back = p.unwrap(store(1), &wrapped).await.unwrap();
        assert_eq!(back.expose(), key.expose());
    }

    #[tokio::test]
    async fn wrong_store_wrong_key_and_damage_do_not_unwrap() {
        let p = LocalKeyProvider::new(MasterKeySet::generate().unwrap());
        let (_, wrapped) = p.generate(store(1)).await.unwrap();
        assert!(matches!(
            p.unwrap(store(2), &wrapped).await,
            Err(KeyError::Unwrap)
        ));

        // Another key set with a key of the same ID cannot exist, so fake one: same ID, other bytes.
        let mut other = MasterKeySet::from_key([9; KEY_LEN]);
        let id = p.keys.current.clone();
        let bytes = other.keys.remove(&other.current).unwrap();
        other.keys.insert(id.clone(), bytes);
        other.current = id;
        let q = LocalKeyProvider::new(other);
        assert!(matches!(
            q.unwrap(store(1), &wrapped).await,
            Err(KeyError::Unwrap)
        ));

        let mut bad = wrapped.clone();
        bad.blob[20] ^= 1;
        assert!(matches!(
            p.unwrap(store(1), &bad).await,
            Err(KeyError::Unwrap)
        ));
        bad.blob.truncate(59);
        assert!(matches!(
            p.unwrap(store(1), &bad).await,
            Err(KeyError::Unwrap)
        ));
    }

    #[tokio::test]
    async fn unknown_master_key() {
        let p = LocalKeyProvider::new(MasterKeySet::generate().unwrap());
        let (_, wrapped) = p.generate(store(1)).await.unwrap();
        let other = LocalKeyProvider::new(MasterKeySet::generate().unwrap());
        match other.unwrap(store(1), &wrapped).await {
            Err(KeyError::UnknownMasterKey { kek_id }) => assert_eq!(kek_id, wrapped.kek_id),
            r => panic!("{r:?}"),
        }
        let kms = WrappedKey {
            kek_id: "arn:aws:kms:eu-west-1:1:key/x".into(),
            blob: wrapped.blob,
        };
        assert!(matches!(
            p.unwrap(store(1), &kms).await,
            Err(KeyError::UnknownMasterKey { .. })
        ));
    }

    #[tokio::test]
    async fn rotation_keeps_old_keys_and_rewraps_under_the_new_one() {
        let mut set = MasterKeySet::generate().unwrap();
        let old_id = set.current_kek_id();
        let old = LocalKeyProvider::new(MasterKeySet::from_json(set.to_json().as_bytes()).unwrap());
        let (key, wrapped) = old.generate(store(3)).await.unwrap();

        set.rotate().unwrap();
        assert_eq!(set.len(), 2);
        assert_ne!(set.current_kek_id(), old_id);
        let saved = MasterKeySet::from_json(set.to_json().as_bytes()).unwrap();
        let p = LocalKeyProvider::new(saved);

        // Rotation: unwrap with the old key, wrap with the new one.
        let dk = p.unwrap(store(3), &wrapped).await.unwrap();
        let rewrapped = p.wrap(store(3), &dk).unwrap();
        assert_eq!(rewrapped.kek_id, p.keys.current_kek_id());
        assert_ne!(rewrapped.kek_id, wrapped.kek_id);
        assert_eq!(
            p.unwrap(store(3), &rewrapped).await.unwrap().expose(),
            key.expose()
        );
        // The old blob still opens: retired keys stay in the set.
        assert!(p.unwrap(store(3), &wrapped).await.is_ok());
        // The old provider cannot open the new blob.
        assert!(matches!(
            old.unwrap(store(3), &rewrapped).await,
            Err(KeyError::UnknownMasterKey { .. })
        ));
    }

    #[test]
    fn key_id_is_the_versioned_sha256_prefix() {
        let key = [0u8; KEY_LEN];
        let mut input = b"knoggin-kek-id-v1".to_vec();
        input.extend_from_slice(&key);
        let full = digest(&SHA256, &input);
        let hex: String = full.as_ref()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(key_id(&key), hex);
        assert_eq!(hex.len(), 16);
    }

    #[test]
    fn json_round_trip_and_exact_shape() {
        let set = MasterKeySet::from_key([0u8; KEY_LEN]);
        let json = set.to_json();
        let id = key_id(&[0u8; KEY_LEN]);
        assert_eq!(
            &*json,
            &format!(
                "{{\"version\":1,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"}}}}"
            )
        );
        let back = MasterKeySet::from_json(json.as_bytes()).unwrap();
        assert_eq!(back.current_kek_id(), set.current_kek_id());
        assert_eq!(back.len(), 1);

        // Whitespace as a person would write it is fine.
        let spaced = format!(
            "{{ \"version\": 1, \"current\": \"{id}\",\n  \"keys\": {{ \"{id}\": \"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\" }} }}\n"
        );
        assert!(MasterKeySet::from_json(spaced.as_bytes()).is_ok());
    }

    #[test]
    fn bad_key_sets_are_refused() {
        let id = key_id(&[0u8; KEY_LEN]);
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let bad = [
            "".to_string(),
            "{}".to_string(),
            "not json".to_string(),
            format!("{{\"version\":2,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"{key}\"}}}}"),
            format!(
                "{{\"version\":1,\"current\":\"0000000000000000\",\"keys\":{{\"{id}\":\"{key}\"}}}}"
            ),
            format!("{{\"version\":1,\"current\":\"{id}\",\"keys\":{{}}}}"),
            format!("{{\"version\":1,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"AAAA\"}}}}"),
            format!("{{\"version\":1,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"{key}AAAA\"}}}}"),
            format!("{{\"version\":1,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"{key}=\"}}}}"),
            format!(
                "{{\"version\":1,\"current\":\"{id}\",\"keys\":{{\"{id}\":\"{key}\"}},\"x\":1}}"
            ),
            // A key ID that does not match the key bytes.
            format!(
                "{{\"version\":1,\"current\":\"0000000000000000\",\"keys\":{{\"0000000000000000\":\"{key}\"}}}}"
            ),
        ];
        for text in bad {
            assert!(
                matches!(
                    MasterKeySet::from_json(text.as_bytes()),
                    Err(KeyError::InvalidKeySet(_))
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn keychain_text_round_trip() {
        let mut set = MasterKeySet::generate().unwrap();
        set.rotate().unwrap();
        let text = set.to_keychain_text();
        assert!(text.len().is_multiple_of(4));
        let back = MasterKeySet::from_keychain_text(format!("{}\n", *text).as_bytes()).unwrap();
        assert_eq!(back.current_kek_id(), set.current_kek_id());
        assert_eq!(back.len(), 2);
        assert!(MasterKeySet::from_keychain_text(b"%%%").is_err());
    }

    #[test]
    fn debug_prints_no_key_bytes() {
        let set = MasterKeySet::from_key([0x41; KEY_LEN]);
        let b64 = encode_key(&[0x41; KEY_LEN]);
        let text = format!("{:?}", LocalKeyProvider::new(set));
        assert!(!text.contains(&*b64), "{text}");
        assert!(text.contains("keys: 1"), "{text}");
    }
}
