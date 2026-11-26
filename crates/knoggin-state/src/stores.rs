// SPDX-License-Identifier: AGPL-3.0-only
//! Stores, their ID index and their name items.

use std::sync::Arc;

use knoggin_core::{Sensitive, StoreId, UserId};

use crate::db::{Cond, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{StoreKind, StoreRecord, StoreRef, UserRef, Versioned, decode};
use crate::seal::FieldKeys;
use crate::typed::{self, RETRIES, after_put, put, read, read_all};
use crate::users::NewStore;

/// A user has at most this many apps, counting those that are scheduled for deletion.
pub const MAX_APPS: usize = 1000;

/// Typed access to stores.
#[derive(Debug, Clone)]
pub struct Stores {
    db: Arc<dyn StateDb>,
    keys: Arc<FieldKeys>,
}

impl Stores {
    /// Over `db`, sealing with `keys`.
    pub fn new(db: Arc<dyn StateDb>, keys: Arc<FieldKeys>) -> Self {
        Self { db, keys }
    }

    /// A store by ID alone, through its ID index (two reads).
    pub async fn get(&self, id: StoreId) -> Result<Option<Versioned<StoreRecord>>, StateError> {
        let Some(r) = read::<UserRef>(&*self.db, None, &keys::store(id), ONE).await? else {
            return Ok(None);
        };
        self.get_of(r.value.user_id, id).await
    }

    /// A store of a known user (one read).
    pub async fn get_of(
        &self,
        user: UserId,
        id: StoreId,
    ) -> Result<Option<Versioned<StoreRecord>>, StateError> {
        read(
            &*self.db,
            Some(&self.keys),
            &keys::user_stores(user),
            &id.to_string(),
        )
        .await
    }

    /// Every store of a user, deleted ones included, in ID order.
    pub async fn list(&self, user: UserId) -> Result<Vec<Versioned<StoreRecord>>, StateError> {
        read_all(&*self.db, &keys::user_stores(user), "")
            .await?
            .into_iter()
            .map(|i| decode(Some(&self.keys), i))
            .collect()
    }

    /// Creates an app: the store, its ID index and its name item, conditional on the name
    /// being free (`AlreadyExists`). A user's 1,001st app is `LimitReached`.
    pub async fn create_app(
        &self,
        user: UserId,
        new: NewStore,
        now: i64,
    ) -> Result<Versioned<StoreRecord>, StateError> {
        // ponytail: two creates at the same moment can both pass the count and end one
        // over the bound; the bound only keeps the partition small, so that is accepted.
        let apps = self
            .list(user)
            .await?
            .iter()
            .filter(|s| s.value.kind == StoreKind::App)
            .count();
        if apps >= MAX_APPS {
            return Err(StateError::LimitReached);
        }
        let record = StoreRecord::new(
            new.id,
            user,
            StoreKind::App,
            new.name.into_inner(),
            &new.wrapped,
            now,
        );
        let writes = store_writes(&self.keys, &record)?;
        let after = after_put(record, &writes[0], None);
        match self.db.write(writes).await {
            Err(StateError::Conflict) => Err(StateError::AlreadyExists),
            other => other.map(|()| after),
        }
    }

    /// Reads the store record, lets `decide` change it and writes it back with
    /// `Version(n)`, retrying at most 5 times, then `Unavailable`. Every write to a store
    /// record goes through here or `rename`. The name cannot change here.
    pub async fn update<R, E: From<StateError>>(
        &self,
        user: UserId,
        id: StoreId,
        decide: impl FnMut(&mut StoreRecord) -> Result<R, E>,
    ) -> Result<(R, Versioned<StoreRecord>), E> {
        typed::update(
            &*self.db,
            Some(&self.keys),
            &keys::user_stores(user),
            &id.to_string(),
            decide,
            |old, new| {
                if old.id != new.id
                    || old.user_id != new.user_id
                    || old.kind != new.kind
                    || old.name != new.name
                {
                    return Err(StateError::Invalid(
                        "a store's identity or name changed outside rename",
                    ));
                }
                Ok(())
            },
        )
        .await
    }

    /// Renames a store and moves its name item in the same write. A name another store
    /// of the user holds is `AlreadyExists`.
    pub async fn rename(
        &self,
        user: UserId,
        id: StoreId,
        name: Sensitive<String>,
    ) -> Result<Versioned<StoreRecord>, StateError> {
        let names = keys::user_store_names(user);
        for _ in 0..RETRIES {
            let current = self.get_of(user, id).await?.ok_or(StateError::NotFound)?;
            let old_hash = keys::name_hash(&self.keys, current.value.name.expose());
            let new_hash = keys::name_hash(&self.keys, name.expose());
            let mut value = current.value.clone();
            value.name = name.clone();
            let w = put(
                Some(&self.keys),
                keys::user_stores(user),
                id.to_string(),
                &value,
                Some(&current),
                current.cond(),
                current.expires_ms,
            )?;
            let after = after_put(value, &w, Some(current.version));
            let mut writes = vec![w];
            if old_hash != new_hash {
                writes.push(Write::delete(&names, &old_hash, Cond::Any));
                writes.push(put(
                    None,
                    names.clone(),
                    new_hash.clone(),
                    &StoreRef { store_id: id },
                    None,
                    Cond::Absent,
                    None,
                )?);
            }
            match self.db.write(writes).await {
                Ok(()) => return Ok(after),
                Err(StateError::Conflict) => {
                    if old_hash != new_hash && self.db.get(&names, &new_hash).await?.is_some() {
                        return Err(StateError::AlreadyExists);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the store record kept changing".into(),
        ))
    }
}

/// The three items of a new store, each conditional on being absent: the record, its ID
/// index and its name item.
pub(crate) fn store_writes(
    keys: &FieldKeys,
    record: &StoreRecord,
) -> Result<Vec<Write>, StateError> {
    Ok(vec![
        put(
            Some(keys),
            keys::user_stores(record.user_id),
            record.id.to_string(),
            record,
            None,
            Cond::Absent,
            None,
        )?,
        put(
            None,
            keys::store(record.id),
            ONE.into(),
            &UserRef {
                user_id: record.user_id,
            },
            None,
            Cond::Absent,
            None,
        )?,
        put(
            None,
            keys::user_store_names(record.user_id),
            keys::name_hash(keys, record.name.expose()),
            &StoreRef {
                store_id: record.id,
            },
            None,
            Cond::Absent,
            None,
        )?,
    ])
}
