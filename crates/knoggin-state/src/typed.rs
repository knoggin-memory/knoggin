// SPDX-License-Identifier: AGPL-3.0-only
//! Reads, writes and the read-decide-write loop that every typed helper shares.

use crate::db::{Cond, Item, StateDb, StateError, Write};
use crate::record::{Record, Versioned, decode, encode};
use crate::seal::FieldKeys;

/// How often a write that lost its `Version(n)` condition reads again before it answers
/// `unavailable`.
pub(crate) const RETRIES: usize = 5;

/// A query page; partitions that must be read whole are read in pages of this size.
pub(crate) const PAGE: usize = 1000;

pub(crate) async fn read<T: Record>(
    db: &dyn StateDb,
    keys: Option<&FieldKeys>,
    pk: &str,
    sk: &str,
) -> Result<Option<Versioned<T>>, StateError> {
    db.get(pk, sk)
        .await?
        .map(|item| decode(keys, item))
        .transpose()
}

/// Every live item of a partition (with the prefix), page by page.
pub(crate) async fn read_all(
    db: &dyn StateDb,
    pk: &str,
    prefix: &str,
) -> Result<Vec<Item>, StateError> {
    let mut out: Vec<Item> = Vec::new();
    loop {
        let after = out.last().map(|i| i.sk.clone());
        let page = db.query(pk, prefix, PAGE, after.as_deref()).await?;
        let done = page.len() < PAGE;
        out.extend(page);
        if done {
            return Ok(out);
        }
    }
}

/// A put of `value`, keeping the sealed strings of `prev` where values did not change.
pub(crate) fn put<T: Record>(
    keys: Option<&FieldKeys>,
    pk: String,
    sk: String,
    value: &T,
    prev: Option<&Versioned<T>>,
    cond: Cond,
    expires_ms: Option<i64>,
) -> Result<Write, StateError> {
    let doc = encode(keys, &pk, &sk, value, prev.map(|p| &p.doc))?;
    Ok(Write::put(
        Item::new(pk, sk, doc).expiring(expires_ms),
        cond,
    ))
}

/// What the item holds after a put of `write` over `prev` succeeded.
pub(crate) fn after_put<T>(value: T, write: &Write, prev_version: Option<u64>) -> Versioned<T> {
    let (doc, expires_ms) = match write {
        Write::Put { item, .. } => (item.doc.clone(), item.expires_ms),
        _ => (serde_json::Value::Null, None),
    };
    Versioned {
        value,
        version: prev_version.map_or(1, |v| v.saturating_add(1)),
        expires_ms,
        doc,
    }
}

/// Reads the record, lets `decide` change it, and writes it with `Version(n)` together
/// with `extra` writes; on a conflict reads and decides again, at most 5 times, then
/// `Unavailable`. `decide` may refuse by returning an error, which ends the loop.
pub(crate) async fn update<T, R, E>(
    db: &dyn StateDb,
    keys: Option<&FieldKeys>,
    pk: &str,
    sk: &str,
    mut decide: impl FnMut(&mut T) -> Result<R, E>,
    guard: impl Fn(&T, &T) -> Result<(), StateError>,
) -> Result<(R, Versioned<T>), E>
where
    T: Record + Clone,
    E: From<StateError>,
{
    for _ in 0..RETRIES {
        let current: Versioned<T> = read(db, keys, pk, sk).await?.ok_or(StateError::NotFound)?;
        let mut value = current.value.clone();
        let result = decide(&mut value)?;
        guard(&current.value, &value)?;
        let w = put(
            keys,
            pk.to_owned(),
            sk.to_owned(),
            &value,
            Some(&current),
            current.cond(),
            current.expires_ms,
        )?;
        let after = after_put(value, &w, Some(current.version));
        match db.write(vec![w]).await {
            Ok(()) => return Ok((result, after)),
            Err(StateError::Conflict) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(StateError::Unavailable("the record kept changing".into()).into())
}

/// A bookkeeping write: applied if the record is still as read, and a conflict is ignored.
pub(crate) async fn bookkeeping(db: &dyn StateDb, writes: Vec<Write>) -> Result<(), StateError> {
    match db.write(writes).await {
        Ok(()) | Err(StateError::Conflict) => Ok(()),
        Err(e) => Err(e),
    }
}
