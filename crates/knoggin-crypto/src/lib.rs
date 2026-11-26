// SPDX-License-Identifier: AGPL-3.0-only
//! Everything in Knoggin that touches key bytes: the snapshot container format, data keys
//! and their wrapping under a master key, the local master key sources, and token helpers.
//!
//! All cryptography comes from `aws-lc-rs`. Key bytes live in types that wipe themselves.

pub mod container;
pub mod key;
pub mod local;
pub mod source;
pub mod testing;

pub use key::{DataKey, KeyError, KeyProvider, WrappedKey};
pub use local::{LocalKeyProvider, MasterKeySet};
