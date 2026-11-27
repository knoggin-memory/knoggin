// SPDX-License-Identifier: AGPL-3.0-only
//! The record side of erasing deleted stores and users, shared by the lifecycle task and
//! the local daemon. It reads and deletes and never changes a record, and it reads no
//! sealed field, so it needs no field key.

use std::future::Future;
use std::sync::Arc;

use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_core::{StoreId, UserId};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::db::{Cond, Item, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::leases::lease_deletes;
use crate::record::Newest;
use crate::sessions::delete_session_pairs;
use crate::typed::{PAGE, read_all};
use crate::users::LOCAL_IDENTITY;

/// An undo is refused once the soft-delete days have passed; erasing waits this much
/// longer, so that two clocks that differ a little never let an erase overtake an undo.
const MARGIN_MS: i64 = 10 * 60 * 1000;

/// What one run did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EraseReport {
    /// Stores whose snapshots and records were erased.
    pub stores_erased: usize,
    /// Users whose records were erased.
    pub users_erased: usize,
    /// Due stores whose snapshots could not be deleted; their records stay for the next run.
    pub stores_failed: usize,
}

#[derive(Deserialize)]
struct UserMarks {
    issuer: String,
    sub_hash: String,
    deleted_at_ms: Option<i64>,
}

#[derive(Deserialize)]
struct StoreMarks {
    id: StoreId,
    deleted_at_ms: Option<i64>,
}

/// What thinning needs of one store that is not marked deleted. Read without the field
/// key, from the record's unsealed fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ThinTarget {
    /// The store.
    #[serde(rename = "id")]
    pub store: StoreId,
    /// The newest committed snapshot the record names.
    pub newest: Option<Newest>,
    /// The snapshot an admin chose to restore, `<generation>-<sequence>`.
    pub restore_from: Option<String>,
    deleted_at_ms: Option<i64>,
}

/// Every store whose own record carries no deletion mark, for the lifecycle task's
/// thinning step. Like [`erase`] it needs no field key.
pub async fn thin_targets(db: &dyn StateDb) -> Result<Vec<ThinTarget>, StateError> {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = db.query(keys::USERS, "", PAGE, after.as_deref()).await?;
        for dir in &page {
            let user: UserId = dir
                .sk
                .parse()
                .map(UserId::from_uuid)
                .map_err(|_| StateError::Corrupt("a user directory item"))?;
            for item in read_all(db, &keys::user_stores(user), "").await? {
                let target: ThinTarget = marks(&item)?;
                if target.deleted_at_ms.is_none() {
                    out.push(target);
                }
            }
        }
        match page.last() {
            Some(last) if page.len() == PAGE => after = Some(last.sk.clone()),
            _ => return Ok(out),
        }
    }
}

#[derive(Deserialize)]
struct KeyMarks {
    hash: String,
}

#[derive(Deserialize)]
struct NameMarks {
    store_id: StoreId,
}

fn marks<T: DeserializeOwned>(item: &Item) -> Result<T, StateError> {
    serde_json::from_value(item.doc.clone()).map_err(|_| StateError::Corrupt("a record"))
}

/// Erases every store whose own deletion mark, or its user's, is more than
/// `soft_delete_days` plus 10 minutes old: first its snapshots through
/// `delete_snapshots`, then its API-key records, then its name item, ID item and record
/// in one write conditional on the record as read. Then erases every user whose mark is
/// that old and who has no store left: sessions, lease, sign-in index and user records.
/// The user with issuer `local` is never erased. A run that stops part-way leaves records
/// that the next run finishes.
pub async fn erase<F, Fut, E>(
    db: Arc<dyn StateDb>,
    now: i64,
    soft_delete_days: u32,
    mut delete_snapshots: F,
) -> Result<EraseReport, StateError>
where
    F: FnMut(StoreId) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let grace = i64::from(soft_delete_days)
        .saturating_mul(MILLIS_PER_DAY)
        .saturating_add(MARGIN_MS);
    let due = |mark: Option<i64>| mark.is_some_and(|t| now.saturating_sub(t) > grace);
    let mut report = EraseReport::default();
    let mut after: Option<String> = None;
    loop {
        let page = db.query(keys::USERS, "", PAGE, after.as_deref()).await?;
        for dir in &page {
            let user: UserId = dir
                .sk
                .parse()
                .map(UserId::from_uuid)
                .map_err(|_| StateError::Corrupt("a user directory item"))?;
            let Some(user_item) = db.get(&keys::user(user), ONE).await? else {
                continue;
            };
            let user_marks: UserMarks = marks(&user_item)?;
            let user_due = due(user_marks.deleted_at_ms);
            let stores_pk = keys::user_stores(user);
            let mut left = 0;
            for item in read_all(&*db, &stores_pk, "").await? {
                let store: StoreMarks = marks(&item)?;
                if !(user_due || due(store.deleted_at_ms)) {
                    left += 1;
                    continue;
                }
                if delete_snapshots(store.id).await.is_err() {
                    report.stores_failed += 1;
                    left += 1;
                    continue;
                }
                if erase_store_records(&*db, user, store.id, item.version).await? {
                    report.stores_erased += 1;
                } else {
                    left += 1;
                }
            }
            if user_due && left == 0 && user_marks.issuer != LOCAL_IDENTITY {
                let sessions = read_all(&*db, &keys::user_sessions(user), "").await?;
                delete_session_pairs(&*db, user, sessions.into_iter().map(|i| i.sk)).await?;
                let mut writes = lease_deletes(user);
                writes.extend([
                    Write::delete(keys::sub(&user_marks.sub_hash), ONE, Cond::Any),
                    Write::delete(keys::USERS, user.to_string(), Cond::Any),
                    Write::delete(keys::user(user), ONE, Cond::Version(user_item.version)),
                ]);
                match db.write(writes).await {
                    Ok(()) => report.users_erased += 1,
                    Err(StateError::Conflict) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        match page.last() {
            Some(last) if page.len() == PAGE => after = Some(last.sk.clone()),
            _ => return Ok(report),
        }
    }
}

/// Deletes a store's key records, then its name item, ID item and record. Returns false
/// if the record changed since it was read (an undo got in first).
async fn erase_store_records(
    db: &dyn StateDb,
    user: UserId,
    store: StoreId,
    version: u64,
) -> Result<bool, StateError> {
    let key_writes: Vec<Write> = read_all(db, &keys::store_keys(store), "")
        .await?
        .iter()
        .map(|i| {
            let k: KeyMarks = marks(i)?;
            Ok([
                Write::delete(keys::key(&k.hash), ONE, Cond::Any),
                Write::delete(i.pk.clone(), i.sk.clone(), Cond::Any),
            ])
        })
        .collect::<Result<Vec<_>, StateError>>()?
        .into_iter()
        .flatten()
        .collect();
    for chunk in key_writes.chunks(24) {
        db.write(chunk.to_vec()).await?;
    }
    let names_pk = keys::user_store_names(user);
    let mut writes = Vec::new();
    for item in read_all(db, &names_pk, "").await? {
        if marks::<NameMarks>(&item)?.store_id == store {
            writes.push(Write::delete(item.pk, item.sk, Cond::Any));
        }
    }
    writes.push(Write::delete(keys::store(store), ONE, Cond::Any));
    writes.push(Write::delete(
        keys::user_stores(user),
        store.to_string(),
        Cond::Version(version),
    ));
    match db.write(writes).await {
        Ok(()) => Ok(true),
        Err(StateError::Conflict) => Ok(false),
        Err(e) => Err(e),
    }
}
