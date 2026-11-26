// SPDX-License-Identifier: AGPL-3.0-only
//! API keys: the `key#<hash>` copy that the credential check reads, and the
//! `store-keys#<store>` copy that lists a store's keys. Both are always written together.

use std::sync::Arc;

use knoggin_core::{Sensitive, StoreId, UserId};
use uuid::Uuid;

use crate::db::{Cond, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{ApiKey, Permission, StoreKey, StoreRecord, Versioned, decode, utc_day};
use crate::seal::FieldKeys;
use crate::typed::{RETRIES, bookkeeping, put, read, read_all};

/// A store has at most this many key records, counting revoked and expired ones.
pub const MAX_KEY_RECORDS: usize = 1000;

/// The parts of a new key that `token::new_api_key` made.
#[derive(Debug, Clone)]
pub struct NewApiKey {
    /// A new UUIDv7.
    pub id: Uuid,
    /// SHA-256 of the whole key string.
    pub hash: [u8; 32],
    /// Its visible prefix.
    pub prefix: String,
    /// When it stops working.
    pub key_expires_ms: i64,
}

/// Typed access to API keys.
#[derive(Debug, Clone)]
pub struct ApiKeys {
    db: Arc<dyn StateDb>,
    keys: Arc<FieldKeys>,
}

impl ApiKeys {
    /// Over `db`, sealing with `keys`.
    pub fn new(db: Arc<dyn StateDb>, keys: Arc<FieldKeys>) -> Self {
        Self { db, keys }
    }

    /// The key whose whole string has this SHA-256. Revoked and expired keys are returned
    /// too until their records expire; `ApiKey::is_active` says whether it works.
    pub async fn get(&self, hash: &[u8; 32]) -> Result<Option<Versioned<ApiKey>>, StateError> {
        read(
            &*self.db,
            Some(&self.keys),
            &keys::key(&keys::secret_hash(hash)),
            ONE,
        )
        .await
    }

    /// Every key record of a store, in key ID order.
    pub async fn list(&self, store: StoreId) -> Result<Vec<Versioned<StoreKey>>, StateError> {
        read_all(&*self.db, &keys::store_keys(store), "")
            .await?
            .into_iter()
            .map(|i| decode(Some(&self.keys), i))
            .collect()
    }

    /// Creates a key: both copies, with a check that the store record is as just read and
    /// not deleted. A store with 1,000 key records is `LimitReached`.
    pub async fn create(
        &self,
        user: UserId,
        store: StoreId,
        new: NewApiKey,
        name: Sensitive<String>,
        permissions: Vec<Permission>,
        now: i64,
    ) -> Result<ApiKey, StateError> {
        self.check_bound(store).await?;
        let key = new_key(user, store, &new, name, permissions, now);
        let hash = keys::secret_hash(&new.hash);
        for _ in 0..RETRIES {
            let mut writes = self.key_writes(&hash, &key, None, None)?;
            writes.push(self.store_check(user, store).await?);
            match self.db.write(writes).await {
                Ok(()) => return Ok(key),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the store record kept changing".into(),
        ))
    }

    /// Makes a new key with the old one's name and permissions and revokes the old one, in
    /// one write that also checks the store record. A revoked or unknown old key is `NotFound`.
    pub async fn rotate(
        &self,
        user: UserId,
        store: StoreId,
        old_id: Uuid,
        new: NewApiKey,
        now: i64,
    ) -> Result<ApiKey, StateError> {
        self.check_bound(store).await?;
        let hash = keys::secret_hash(&new.hash);
        for _ in 0..RETRIES {
            let (old_store, old_key) = self.read_pair(store, old_id).await?;
            if old_key.value.revoked_ms.is_some() {
                return Err(StateError::NotFound);
            }
            let key = new_key(
                user,
                store,
                &new,
                old_key.value.name.clone(),
                old_key.value.permissions.clone(),
                now,
            );
            let mut revoked = old_key.value.clone();
            revoked.revoked_ms = Some(now);
            let mut writes = self.key_writes(&hash, &key, None, None)?;
            writes.extend(self.key_writes(
                &old_store.value.hash,
                &revoked,
                Some(&old_key),
                Some(&old_store),
            )?);
            writes.push(self.store_check(user, store).await?);
            match self.db.write(writes).await {
                Ok(()) => return Ok(key),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the key records kept changing".into(),
        ))
    }

    /// Revokes a key: both copies get `revoked_ms`, and their records expire 30 days later.
    /// Revoking a revoked key changes nothing.
    pub async fn revoke(
        &self,
        store: StoreId,
        key_id: Uuid,
        now: i64,
    ) -> Result<ApiKey, StateError> {
        for _ in 0..RETRIES {
            let (in_store, by_hash) = self.read_pair(store, key_id).await?;
            if by_hash.value.revoked_ms.is_some() {
                return Ok(by_hash.value);
            }
            let mut key = by_hash.value.clone();
            key.revoked_ms = Some(now);
            let writes =
                self.key_writes(&in_store.value.hash, &key, Some(&by_hash), Some(&in_store))?;
            match self.db.write(writes).await {
                Ok(()) => return Ok(key),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the key records kept changing".into(),
        ))
    }

    /// Writes `last_used_day` to both copies when it is not today, conditional on the
    /// `key#` copy as read; a conflict is ignored, so a revocation is never undone.
    pub async fn touch_used(
        &self,
        key: &Versioned<ApiKey>,
        hash: &[u8; 32],
        now: i64,
    ) -> Result<(), StateError> {
        let today = utc_day(now);
        if key.value.last_used_day.as_deref() == Some(today.as_str()) {
            return Ok(());
        }
        let in_store: Option<Versioned<StoreKey>> = read(
            &*self.db,
            Some(&self.keys),
            &keys::store_keys(key.value.store_id),
            &key.value.id.to_string(),
        )
        .await?;
        let Some(in_store) = in_store else {
            return Ok(());
        };
        let mut value = key.value.clone();
        value.last_used_day = Some(today);
        let mut writes =
            self.key_writes(&keys::secret_hash(hash), &value, Some(key), Some(&in_store))?;
        if let Some(Write::Put { cond, .. }) = writes.get_mut(1) {
            *cond = Cond::Any;
        }
        bookkeeping(&*self.db, writes).await
    }

    async fn check_bound(&self, store: StoreId) -> Result<(), StateError> {
        let n = self
            .db
            .query(&keys::store_keys(store), "", MAX_KEY_RECORDS, None)
            .await?
            .len();
        if n >= MAX_KEY_RECORDS {
            return Err(StateError::LimitReached);
        }
        Ok(())
    }

    /// A check that the store record is as it is now, refusing a deleted store.
    async fn store_check(&self, user: UserId, store: StoreId) -> Result<Write, StateError> {
        let rec: Versioned<StoreRecord> = read(
            &*self.db,
            Some(&self.keys),
            &keys::user_stores(user),
            &store.to_string(),
        )
        .await?
        .ok_or(StateError::NotFound)?;
        if rec.value.deleted_at_ms.is_some() {
            return Err(StateError::StoreDeleted);
        }
        Ok(Write::check(
            keys::user_stores(user),
            store.to_string(),
            rec.cond(),
        ))
    }

    async fn read_pair(
        &self,
        store: StoreId,
        key_id: Uuid,
    ) -> Result<(Versioned<StoreKey>, Versioned<ApiKey>), StateError> {
        let in_store: Versioned<StoreKey> = read(
            &*self.db,
            Some(&self.keys),
            &keys::store_keys(store),
            &key_id.to_string(),
        )
        .await?
        .ok_or(StateError::NotFound)?;
        let by_hash: Versioned<ApiKey> = read(
            &*self.db,
            Some(&self.keys),
            &keys::key(&in_store.value.hash),
            ONE,
        )
        .await?
        .ok_or(StateError::NotFound)?;
        Ok((in_store, by_hash))
    }

    /// The two copies of `key`, each `Absent` when new and `Version(n)` over a read copy.
    fn key_writes(
        &self,
        hash: &str,
        key: &ApiKey,
        by_hash: Option<&Versioned<ApiKey>>,
        in_store: Option<&Versioned<StoreKey>>,
    ) -> Result<Vec<Write>, StateError> {
        let expires = Some(key.item_expires_ms());
        let store_copy = StoreKey {
            hash: hash.to_owned(),
            key: key.clone(),
        };
        Ok(vec![
            put(
                Some(&self.keys),
                keys::key(hash),
                ONE.into(),
                key,
                by_hash,
                crate::record::cond_of(by_hash),
                expires,
            )?,
            put(
                Some(&self.keys),
                keys::store_keys(key.store_id),
                key.id.to_string(),
                &store_copy,
                in_store,
                crate::record::cond_of(in_store),
                expires,
            )?,
        ])
    }
}

fn new_key(
    user: UserId,
    store: StoreId,
    new: &NewApiKey,
    name: Sensitive<String>,
    permissions: Vec<Permission>,
    now: i64,
) -> ApiKey {
    ApiKey {
        id: new.id,
        store_id: store,
        user_id: user,
        name,
        prefix: new.prefix.clone(),
        permissions,
        created_ms: now,
        key_expires_ms: new.key_expires_ms,
        last_used_day: None,
        revoked_ms: None,
    }
}
