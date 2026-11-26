// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers: a fixed master key set.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_key_never_changes() {
        assert_eq!(test_key_set().current_kek_id(), "local:dd369de2a0cd2406");
    }
}
