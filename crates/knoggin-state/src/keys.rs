// SPDX-License-Identifier: AGPL-3.0-only
//! Every key layout of the state database. No code outside this crate builds a key.

use std::fmt::Display;

use crate::seal::{FieldKeys, hex};

/// The sort key of a partition with one item.
pub(crate) const ONE: &str = "-";

pub(crate) fn user(id: impl Display) -> String {
    format!("user#{id}")
}
pub(crate) const USERS: &str = "users";
pub(crate) fn sub(hash: &str) -> String {
    format!("sub#{hash}")
}
pub(crate) fn user_stores(user: impl Display) -> String {
    format!("user-stores#{user}")
}
pub(crate) fn store(id: impl Display) -> String {
    format!("store#{id}")
}
pub(crate) fn user_store_names(user: impl Display) -> String {
    format!("user-store-names#{user}")
}
pub(crate) fn key(hash: &str) -> String {
    format!("key#{hash}")
}
pub(crate) fn store_keys(store: impl Display) -> String {
    format!("store-keys#{store}")
}
pub(crate) fn session(hash: &str) -> String {
    format!("session#{hash}")
}
pub(crate) fn user_sessions(user: impl Display) -> String {
    format!("user-sessions#{user}")
}
pub(crate) fn lease(user: impl Display) -> String {
    format!("lease#{user}")
}
pub(crate) const ACTIVE_LEASES: &str = "active-leases";
pub(crate) const CONTAINERS: &str = "containers";
pub(crate) const LEADER: (&str, &str) = ("leader", "reconcile");
pub(crate) const META: &str = "meta";
pub(crate) const ROLLOUT: &str = "rollout";
pub(crate) const FIELD_KEY: &str = "field-key";
pub(crate) const SCHEMA: &str = "schema";

/// Hex HMAC of issuer, a newline and subject: the hash in `sub#<hash>`.
pub(crate) fn sub_hash(keys: &FieldKeys, issuer: &str, subject: &str) -> String {
    keys.index_hmac(format!("{issuer}\n{subject}").as_bytes())
}

/// Hex HMAC of the lower-cased name: the sort key of a store name item.
pub(crate) fn name_hash(keys: &FieldKeys, name: &str) -> String {
    keys.index_hmac(name.to_lowercase().as_bytes())
}

/// Hex SHA-256 of a key or a cookie value, as the key of its record.
pub(crate) fn secret_hash(hash: &[u8; 32]) -> String {
    hex(hash)
}
