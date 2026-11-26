// SPDX-License-Identifier: AGPL-3.0-only
//! The `meta` partition (schema, field key, rollout), the reconcile leader, and
//! re-wrapping every key under a new master key.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knoggin_core::{StoreId, UserId};
use knoggin_crypto::{KeyProvider, LocalKeyProvider, WrappedKey};

use crate::db::{Cond, StateDb, StateError};
use crate::keys;
use crate::record::{FieldKeyRecord, Leader, Rollout, Schema, Versioned};
use crate::seal::FieldKeys;
use crate::stores::Stores;
use crate::typed::{put, read};
use crate::users::Users;

/// The schema version this build writes and the highest it runs against.
pub const SCHEMA_VERSION: u64 = 1;

/// Typed access to the `meta` partition and the reconcile leader.
#[derive(Debug, Clone)]
pub struct Meta {
    db: Arc<dyn StateDb>,
}

impl Meta {
    /// Over `db`.
    pub fn new(db: Arc<dyn StateDb>) -> Self {
        Self { db }
    }

    /// Writes the schema record if it is missing, and refuses a newer schema. This read
    /// is also the health check.
    pub async fn check_schema(&self) -> Result<(), StateError> {
        for _ in 0..2 {
            match read::<Schema>(&*self.db, None, keys::META, keys::SCHEMA).await? {
                Some(s) if s.value.version > SCHEMA_VERSION => {
                    return Err(StateError::SchemaTooNew(s.value.version));
                }
                Some(_) => return Ok(()),
                None => {
                    let w = put(
                        None,
                        keys::META.into(),
                        keys::SCHEMA.into(),
                        &Schema {
                            version: SCHEMA_VERSION,
                        },
                        None,
                        Cond::Absent,
                        None,
                    )?;
                    match self.db.write(vec![w]).await {
                        Ok(()) => return Ok(()),
                        Err(StateError::Conflict) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Err(StateError::Unavailable(
            "the schema record kept changing".into(),
        ))
    }

    /// Unwraps the field key. On an empty deployment (no user yet) it first makes one with
    /// `KeyProvider::generate` under the nil store ID and writes it with `Absent`; if users
    /// exist without it, `FieldKeyMissing`.
    pub async fn field_keys(&self, provider: &dyn KeyProvider) -> Result<FieldKeys, StateError> {
        for _ in 0..2 {
            if let Some(r) = self.field_key_record().await? {
                let wrapped = field_wrapped(&r.value)?;
                let key = provider.unwrap(StoreId::default(), &wrapped).await?;
                return Ok(FieldKeys::new(&key));
            }
            if !self.db.query(keys::USERS, "", 1, None).await?.is_empty() {
                return Err(StateError::FieldKeyMissing);
            }
            let (key, wrapped) = provider.generate(StoreId::default()).await?;
            match self
                .db
                .write(vec![field_write(&wrapped, Cond::Absent)?])
                .await
            {
                Ok(()) => return Ok(FieldKeys::new(&key)),
                // Another process made one at the same moment: use theirs.
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the field key record kept changing".into(),
        ))
    }

    async fn field_key_record(&self) -> Result<Option<Versioned<FieldKeyRecord>>, StateError> {
        read(&*self.db, None, keys::META, keys::FIELD_KEY).await
    }

    /// The rollout record.
    pub async fn rollout(&self) -> Result<Option<Versioned<Rollout>>, StateError> {
        read(&*self.db, None, keys::META, keys::ROLLOUT).await
    }

    /// Writes the rollout record under `cond`.
    pub async fn put_rollout(&self, rollout: &Rollout, cond: Cond) -> Result<(), StateError> {
        let w = put(
            None,
            keys::META.into(),
            keys::ROLLOUT.into(),
            rollout,
            None,
            cond,
            None,
        )?;
        self.db.write(vec![w]).await
    }

    /// The reconcile leader; `None` once its `until_ms` has passed.
    pub async fn leader(&self) -> Result<Option<Versioned<Leader>>, StateError> {
        read(&*self.db, None, keys::LEADER.0, keys::LEADER.1).await
    }

    /// Takes or renews leadership under `cond` (`Absent` when there is no live leader).
    /// The item expires at `until_ms`.
    pub async fn put_leader(&self, leader: &Leader, cond: Cond) -> Result<(), StateError> {
        let w = put(
            None,
            keys::LEADER.0.into(),
            keys::LEADER.1.into(),
            leader,
            None,
            cond,
            Some(leader.until_ms),
        )?;
        self.db.write(vec![w]).await
    }
}

fn field_wrapped(r: &FieldKeyRecord) -> Result<WrappedKey, StateError> {
    Ok(WrappedKey {
        kek_id: r.kek_id.clone(),
        blob: URL_SAFE_NO_PAD
            .decode(&r.wrapped_key)
            .map_err(|_| StateError::Corrupt("the field key record"))?,
    })
}

fn field_write(wrapped: &WrappedKey, cond: Cond) -> Result<crate::db::Write, StateError> {
    put(
        None,
        keys::META.into(),
        keys::FIELD_KEY.into(),
        &FieldKeyRecord {
            wrapped_key: URL_SAFE_NO_PAD.encode(&wrapped.blob),
            kek_id: wrapped.kek_id.clone(),
        },
        None,
        cond,
        None,
    )
}

/// The record side of master-key rotation (`knoggin key rotate`, `knoggin admin
/// rotate-master-key`), with the process stopped: wraps the field key and every store's
/// data key again under the provider's current master key. `provider` holds the new key
/// as `current` and the old ones beside it. Returns the number of stores re-wrapped.
/// `LocalFileDb::rewrap` does the same for the state file's own key.
pub async fn rewrap_master_key(
    db: Arc<dyn StateDb>,
    keys: Arc<FieldKeys>,
    provider: &LocalKeyProvider,
) -> Result<usize, StateError> {
    let meta = Meta::new(db.clone());
    let record = meta
        .field_key_record()
        .await?
        .ok_or(StateError::FieldKeyMissing)?;
    let field = provider
        .unwrap(StoreId::default(), &field_wrapped(&record.value)?)
        .await?;
    let wrapped = provider.wrap(StoreId::default(), &field)?;
    db.write(vec![field_write(&wrapped, record.cond())?])
        .await?;

    let users = Users::new(db.clone(), keys.clone());
    let stores = Stores::new(db.clone(), keys);
    let mut count = 0;
    let mut after: Option<String> = None;
    loop {
        // Paged by the directory itself: a directory item whose user record is gone
        // shortens no page, so it cannot end the walk early.
        let page = db
            .query(keys::USERS, "", crate::typed::PAGE, after.as_deref())
            .await?;
        for item in &page {
            let user = item
                .sk
                .parse()
                .map(UserId::from_uuid)
                .map_err(|_| StateError::Corrupt("a user directory item"))?;
            if users.get(user).await?.is_none() {
                continue;
            }
            for store in stores.list(user).await? {
                let id = store.value.id;
                let data_key = provider.unwrap(id, &store.value.wrapped()?).await?;
                let wrapped = provider.wrap(id, &data_key)?;
                stores
                    .update(user, id, |s| {
                        s.set_wrapped(&wrapped);
                        Ok::<_, StateError>(())
                    })
                    .await?;
                count += 1;
            }
        }
        match page.last() {
            Some(last) if page.len() == crate::typed::PAGE => after = Some(last.sk.clone()),
            _ => return Ok(count),
        }
    }
}

#[cfg(test)]
mod tests {
    use knoggin_core::Sensitive;
    use knoggin_crypto::testing::{test_key_set, test_provider};
    use uuid::Uuid;

    use super::*;
    use crate::db::{Item, Write};
    use crate::{LocalFileDb, NewStore, NewUser};

    #[tokio::test]
    async fn rotation_walks_past_a_page_of_directory_items_without_users() {
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(crate::system_clock()));
        let provider = test_provider();
        let keys = Arc::new(Meta::new(db.clone()).field_keys(&provider).await.unwrap());
        // A whole first page of directory items whose user records are gone.
        let dangling: Vec<Write> = (0..crate::typed::PAGE as u128)
            .map(|i| {
                let id = Uuid::from_u128(i);
                Write::put(
                    Item::new(keys::USERS, id.to_string(), serde_json::json!({})),
                    Cond::Any,
                )
            })
            .collect();
        for chunk in dangling.chunks(25) {
            db.write(chunk.to_vec()).await.unwrap();
        }
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = provider.generate(store).await.unwrap();
        Users::new(db.clone(), keys.clone())
            .create(
                NewUser {
                    id: UserId::from_uuid(Uuid::now_v7()),
                    issuer: "local".into(),
                    subject: Sensitive::new("local".into()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                0,
            )
            .await
            .unwrap();
        let mut set = test_key_set();
        set.rotate().unwrap();
        let rotated = LocalKeyProvider::new(set);
        let n = rewrap_master_key(db, keys, &rotated).await.unwrap();
        assert_eq!(n, 1, "the store after the dangling page is re-wrapped");
    }
}
