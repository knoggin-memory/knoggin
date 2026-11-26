// SPDX-License-Identifier: AGPL-3.0-only
//! The `StateDb` interface: a key-value store with two-part keys and conditional,
//! all-or-nothing writes. Also the in-memory table that `LocalFileDb` keeps.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use knoggin_crypto::KeyError;

/// Most writes in one call of `StateDb::write` (DynamoDB's transaction limit is higher, but
/// the records never need more).
pub const MAX_WRITES: usize = 25;

/// The current time in milliseconds since 1970. Backends compare expiry against it, so a
/// test passes a clock it controls.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The real clock.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    })
}

/// One item. `version` counts the puts; callers never set it (a put ignores it).
#[derive(Clone, PartialEq)]
pub struct Item {
    /// Partition key.
    pub pk: String,
    /// Sort key; `-` when the partition has one item.
    pub sk: String,
    /// 1 for a new item, one more on every put.
    pub version: u64,
    /// The item does not exist once `expires_ms <= now`.
    pub expires_ms: Option<i64>,
    /// The JSON document. Personal fields in it are already sealed.
    pub doc: serde_json::Value,
}

impl Item {
    /// An item to put; the version is set by the database.
    pub fn new(pk: impl Into<String>, sk: impl Into<String>, doc: serde_json::Value) -> Self {
        Self {
            pk: pk.into(),
            sk: sk.into(),
            version: 0,
            expires_ms: None,
            doc,
        }
    }

    /// The same item with an expiry.
    pub fn expiring(mut self, expires_ms: Option<i64>) -> Self {
        self.expires_ms = expires_ms;
        self
    }

    fn live(&self, now: i64) -> bool {
        self.expires_ms.is_none_or(|e| e > now)
    }
}

impl fmt::Debug for Item {
    // The document can hold sealed values and counts; only the keys are printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Item")
            .field("pk", &self.pk)
            .field("sk", &self.sk)
            .field("version", &self.version)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// The condition one write depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    /// No condition.
    Any,
    /// The item must not exist (an expired item does not).
    Absent,
    /// The item must exist with this version.
    Version(u64),
}

/// One part of a `StateDb::write`.
#[derive(Debug, Clone)]
pub enum Write {
    /// Store the item. A new item gets version 1, an existing one its version plus 1.
    Put {
        /// The item; its `version` is ignored.
        item: Item,
        /// The condition.
        cond: Cond,
    },
    /// Remove the item. `Cond::Absent` makes no sense here and is refused.
    Delete {
        /// Partition key.
        pk: String,
        /// Sort key.
        sk: String,
        /// The condition.
        cond: Cond,
    },
    /// Writes nothing; the whole write fails unless the condition holds. `Cond::Any` is refused.
    Check {
        /// Partition key.
        pk: String,
        /// Sort key.
        sk: String,
        /// The condition.
        cond: Cond,
    },
}

impl Write {
    /// A put.
    pub fn put(item: Item, cond: Cond) -> Self {
        Self::Put { item, cond }
    }

    /// A delete.
    pub fn delete(pk: impl Into<String>, sk: impl Into<String>, cond: Cond) -> Self {
        Self::Delete {
            pk: pk.into(),
            sk: sk.into(),
            cond,
        }
    }

    /// A check.
    pub fn check(pk: impl Into<String>, sk: impl Into<String>, cond: Cond) -> Self {
        Self::Check {
            pk: pk.into(),
            sk: sk.into(),
            cond,
        }
    }

    /// The item this write names.
    pub fn key(&self) -> (&str, &str) {
        match self {
            Self::Put { item, .. } => (&item.pk, &item.sk),
            Self::Delete { pk, sk, .. } | Self::Check { pk, sk, .. } => (pk, sk),
        }
    }

    fn cond(&self) -> Cond {
        match self {
            Self::Put { cond, .. } | Self::Delete { cond, .. } | Self::Check { cond, .. } => *cond,
        }
    }
}

/// Errors of the state database and its typed helpers.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// A condition failed; nothing was written.
    #[error("a condition of the write failed")]
    Conflict,
    /// The database cannot be read or written now; nothing was written. Try again later.
    #[error("state database unavailable: {0}")]
    Unavailable(String),
    /// A task of the backend panicked.
    #[error("internal error in the state database")]
    Internal,
    /// The call breaks a rule of the interface: more than 25 writes, one item named twice,
    /// an empty key, a delete with `Absent` or a check with `Any`. A bug of the caller.
    #[error("invalid state database call: {0}")]
    Invalid(&'static str),
    /// A record, a sealed field or the state file does not decode or does not open.
    #[error("corrupt state: {0}")]
    Corrupt(&'static str),
    /// The master key cannot unwrap a key that the state database needs.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// The schema record names a version this build does not know.
    #[error("the state database has schema version {0}, newer than this build")]
    SchemaTooNew(u64),
    /// Users exist but the field key record does not: a new key could not open their records.
    #[error("the field key record is missing")]
    FieldKeyMissing,
    /// The record does not exist.
    #[error("not found")]
    NotFound,
    /// A record with this unique name exists (`already_exists`).
    #[error("already exists")]
    AlreadyExists,
    /// 1,000 apps per user or 1,000 key records per store (`conflict`).
    #[error("limit reached")]
    LimitReached,
    /// The store is marked deleted (`store_deleting`).
    #[error("the store is marked deleted")]
    StoreDeleted,
}

/// The state database. Used as `Arc<dyn StateDb>`; reads are strongly consistent.
#[async_trait]
pub trait StateDb: Send + Sync + fmt::Debug {
    /// One item, or `None` if it is missing or expired.
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, StateError>;
    /// Up to `limit` live items of partition `pk` whose sort key starts with `sk_prefix`,
    /// in byte order of `sk`, after `after`. Fewer than `limit` means there are no more.
    async fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<Item>, StateError>;
    /// Applies up to 25 writes all or nothing. A failed condition is `Conflict`.
    async fn write(&self, writes: Vec<Write>) -> Result<(), StateError>;
}

/// The rules every backend checks before it touches anything.
pub fn validate(writes: &[Write]) -> Result<(), StateError> {
    if writes.len() > MAX_WRITES {
        return Err(StateError::Invalid("more than 25 writes"));
    }
    let mut seen = HashSet::new();
    for w in writes {
        let (pk, sk) = w.key();
        if pk.is_empty() || sk.is_empty() {
            return Err(StateError::Invalid("empty key"));
        }
        if !seen.insert((pk, sk)) {
            return Err(StateError::Invalid("one item named twice"));
        }
        match (w, w.cond()) {
            (Write::Delete { .. }, Cond::Absent) => {
                return Err(StateError::Invalid("a delete with Absent"));
            }
            (Write::Check { .. }, Cond::Any) => {
                return Err(StateError::Invalid("a check with Any"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The items of `LocalFileDb`, ordered by `(pk, sk)`; `String` order is byte order.
pub(crate) type Table = BTreeMap<(String, String), Item>;

pub(crate) fn table_get(table: &Table, pk: &str, sk: &str, now: i64) -> Option<Item> {
    table
        .get(&(pk.to_owned(), sk.to_owned()))
        .filter(|i| i.live(now))
        .cloned()
}

pub(crate) fn table_query(
    table: &Table,
    pk: &str,
    prefix: &str,
    limit: usize,
    after: Option<&str>,
    now: i64,
) -> Vec<Item> {
    let start = (pk.to_owned(), prefix.to_owned());
    table
        .range(start..)
        .take_while(|((p, s), _)| p == pk && s.starts_with(prefix))
        .filter(|((_, s), i)| after.is_none_or(|a| s.as_str() > a) && i.live(now))
        .take(limit)
        .map(|(_, i)| i.clone())
        .collect()
}

/// Applies `writes` to a copy of `table` and returns it, leaving `table` alone. Expired
/// items are left out of the copy.
pub(crate) fn table_apply(
    table: &Table,
    writes: Vec<Write>,
    now: i64,
) -> Result<Table, StateError> {
    validate(&writes)?;
    for w in &writes {
        let (pk, sk) = w.key();
        let current = table_get(table, pk, sk, now);
        let holds = match w.cond() {
            Cond::Any => true,
            Cond::Absent => current.is_none(),
            Cond::Version(n) => current.is_some_and(|i| i.version == n),
        };
        if !holds {
            return Err(StateError::Conflict);
        }
    }
    let mut next: Table = table
        .iter()
        .filter(|(_, i)| i.live(now))
        .map(|(k, i)| (k.clone(), i.clone()))
        .collect();
    for w in writes {
        match w {
            Write::Put { mut item, .. } => {
                let key = (item.pk.clone(), item.sk.clone());
                item.version = next
                    .get(&key)
                    .map_or(1, |old| old.version.saturating_add(1));
                next.insert(key, item);
            }
            Write::Delete { pk, sk, .. } => {
                next.remove(&(pk, sk));
            }
            Write::Check { .. } => {}
        }
    }
    Ok(next)
}
