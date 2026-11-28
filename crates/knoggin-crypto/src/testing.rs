// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers: a fixed master key set, and helpers that seal and open whole snapshot files.
//!
//! The frozen fixtures under `tests/fixtures/snapshots/v1/` are sealed with this key set, so
//! its key must never change.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::error::Error;

use knoggin_core::StoreId;

use crate::container::{self, Header};
use crate::key::{DataKey, KeyScope};
use crate::local::{LocalKeyProvider, MasterKeySet};

/// The one key of the test key set. Public on purpose: it protects nothing.
pub const TEST_MASTER_KEY: [u8; 32] = *b"knoggin-test-master-key-00000001";

/// The test key set: `TEST_MASTER_KEY` alone, as `current`.
pub fn test_key_set() -> MasterKeySet {
    MasterKeySet::from_key(TEST_MASTER_KEY)
}

/// A `LocalKeyProvider` over the test key set.
pub fn test_provider() -> LocalKeyProvider {
    LocalKeyProvider::new(test_key_set())
}

/// Seals `content` under `key` and returns the whole file.
pub fn seal_to_vec(header: Header, key: DataKey, content: Vec<u8>) -> Vec<u8> {
    container::seal(header, key, content)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .concat()
}

/// Makes a new data key for `store` and `generation` under the test key set and seals
/// `content` with it, as a store's snapshot. Returns the whole file.
pub fn seal_snapshot(
    store: StoreId,
    generation: u64,
    sequence: u64,
    created_ms: u64,
    content_format: u32,
    content: Vec<u8>,
) -> Vec<u8> {
    let (key, wrapped) = test_provider()
        .generate_key(KeyScope { store, generation })
        .unwrap();
    let header = Header::new(
        store,
        generation,
        sequence,
        created_ms,
        content_format,
        wrapped,
    );
    seal_to_vec(header, key, content)
}

/// Opens a whole file: unwrap the header's data key with the test key set, for the header's
/// store and generation, decrypt, and return the header with the content. Checks no
/// expected name.
pub fn open_snapshot(bytes: &[u8]) -> Result<(Header, Vec<u8>), Box<dyn Error>> {
    let (header, n) = container::read_header(bytes)?;
    let scope = KeyScope {
        store: header.store_id,
        generation: header.generation,
    };
    let key = test_provider().unwrap_key(scope, &header.wrapped_key())?;
    let content = container::open(&header, &key, &bytes[n..], u64::MAX)?;
    Ok((header, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_key_never_changes() {
        assert_eq!(test_key_set().current_kek_id(), "local:dd369de2a0cd2406");
    }

    #[test]
    fn seal_and_open_a_snapshot() {
        let store = StoreId::from_bytes([3; 16]);
        let bytes = seal_snapshot(store, 1, 2, 1_700_000_000_000, 1, b"content".to_vec());
        let (header, content) = open_snapshot(&bytes).unwrap();
        header
            .expect(
                store,
                knoggin_core::snapshot::SnapshotName::new(1, 2).unwrap(),
            )
            .unwrap();
        assert_eq!(header.kek_id, test_key_set().current_kek_id());
        assert_eq!(content, b"content");
        let mut bad = bytes.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open_snapshot(&bad).is_err());
    }
}
