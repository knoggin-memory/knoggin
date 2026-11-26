// SPDX-License-Identifier: AGPL-3.0-only
//! Users, the user directory and the sign-in index.

use std::sync::Arc;

use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::WrappedKey;

use crate::db::{Cond, Item, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{StoreKind, StoreRecord, User, UserRef, Versioned, utc_day};
use crate::seal::FieldKeys;
use crate::stores::store_writes;
use crate::typed::{self, after_put, bookkeeping, put, read};

/// The issuer and subject of the one user of a local install or a server without sign-in.
pub const LOCAL_IDENTITY: &str = "local";

/// A person seen for the first time.
#[derive(Debug, Clone)]
pub struct NewUser {
    /// A new UUIDv7.
    pub id: UserId,
    /// The sign-in issuer.
    pub issuer: String,
    /// The sign-in subject.
    pub subject: Sensitive<String>,
    /// The email, if the provider sent one.
    pub email: Option<Sensitive<String>>,
    /// Whether the provider said it was verified.
    pub email_verified: bool,
    /// The display name.
    pub name: Option<Sensitive<String>>,
}

/// A new store with its new data key.
#[derive(Debug, Clone)]
pub struct NewStore {
    /// A new UUIDv7.
    pub id: StoreId,
    /// Its name, unique per user ignoring case.
    pub name: Sensitive<String>,
    /// The data key that `KeyProvider::generate` made for this ID.
    pub wrapped: WrappedKey,
}

/// Typed access to users.
#[derive(Debug, Clone)]
pub struct Users {
    db: Arc<dyn StateDb>,
    keys: Arc<FieldKeys>,
}

impl Users {
    /// Over `db`, sealing with `keys`.
    pub fn new(db: Arc<dyn StateDb>, keys: Arc<FieldKeys>) -> Self {
        Self { db, keys }
    }

    /// One user.
    pub async fn get(&self, id: UserId) -> Result<Option<Versioned<User>>, StateError> {
        read(&*self.db, Some(&self.keys), &keys::user(id), ONE).await
    }

    /// The user of a sign-in identity, through the sign-in index.
    pub async fn find(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<Versioned<User>>, StateError> {
        let hash = keys::sub_hash(&self.keys, issuer, subject);
        let Some(r) = read::<UserRef>(&*self.db, None, &keys::sub(&hash), ONE).await? else {
            return Ok(None);
        };
        self.get(r.value.user_id).await
    }

    /// Creates the user with its personal store in one write, conditional on the sign-in
    /// index being absent. If another process created the same person first, returns that
    /// user and `false`.
    pub async fn create(
        &self,
        new: NewUser,
        personal: NewStore,
        now: i64,
    ) -> Result<(Versioned<User>, bool), StateError> {
        let sub_hash = keys::sub_hash(&self.keys, &new.issuer, new.subject.expose());
        let user = User {
            id: new.id,
            issuer: new.issuer.clone(),
            subject: new.subject.clone(),
            sub_hash: sub_hash.clone(),
            email: new.email,
            email_verified: new.email_verified,
            name: new.name,
            personal_store_id: personal.id,
            created_ms: now,
            last_active_day: None,
            deleted_at_ms: None,
            deleted_by: None,
        };
        let user_write = put(
            Some(&self.keys),
            keys::user(new.id),
            ONE.into(),
            &user,
            None,
            Cond::Absent,
            None,
        )?;
        let after = after_put(user, &user_write, None);
        let store = personal_record(new.id, personal, now);
        let mut writes = vec![
            user_write,
            Write::put(
                Item::new(keys::USERS, new.id.to_string(), serde_json::json!({})),
                Cond::Absent,
            ),
            put(
                None,
                keys::sub(&sub_hash),
                ONE.into(),
                &UserRef { user_id: new.id },
                None,
                Cond::Absent,
                None,
            )?,
        ];
        writes.extend(store_writes(&self.keys, &store)?);
        match self.db.write(writes).await {
            Ok(()) => Ok((after, true)),
            Err(StateError::Conflict) => {
                let winner = self.find(&new.issuer, new.subject.expose()).await?;
                winner.map(|u| (u, false)).ok_or(StateError::Conflict)
            }
            Err(e) => Err(e),
        }
    }

    /// Up to `limit` users after `after`, in ID order.
    pub async fn list(
        &self,
        limit: usize,
        after: Option<UserId>,
    ) -> Result<Vec<Versioned<User>>, StateError> {
        let after = after.map(|a| a.to_string());
        let page = self
            .db
            .query(keys::USERS, "", limit, after.as_deref())
            .await?;
        let mut out = Vec::with_capacity(page.len());
        for item in page {
            let id = item
                .sk
                .parse()
                .map(UserId::from_uuid)
                .map_err(|_| StateError::Corrupt("a user directory item"))?;
            if let Some(u) = self.get(id).await? {
                out.push(u);
            }
        }
        Ok(out)
    }

    /// Reads the user, lets `decide` change it and writes it back with `Version(n)`,
    /// retrying at most 5 times. The identity and the personal store cannot change here.
    pub async fn update<R, E: From<StateError>>(
        &self,
        id: UserId,
        decide: impl FnMut(&mut User) -> Result<R, E>,
    ) -> Result<(R, Versioned<User>), E> {
        typed::update(
            &*self.db,
            Some(&self.keys),
            &keys::user(id),
            ONE,
            decide,
            |old, new| {
                if old.id != new.id
                    || old.issuer != new.issuer
                    || old.subject != new.subject
                    || old.sub_hash != new.sub_hash
                    || old.personal_store_id != new.personal_store_id
                {
                    return Err(StateError::Invalid(
                        "a user's identity or personal store changed",
                    ));
                }
                Ok(())
            },
        )
        .await
    }

    /// Writes `last_active_day` when it is not today, if the user is still as read.
    pub async fn touch_active(&self, user: &Versioned<User>, now: i64) -> Result<(), StateError> {
        let today = utc_day(now);
        if user.value.last_active_day.as_deref() == Some(today.as_str()) {
            return Ok(());
        }
        let mut value = user.value.clone();
        value.last_active_day = Some(today);
        let w = put(
            Some(&self.keys),
            keys::user(value.id),
            ONE.into(),
            &value,
            Some(user),
            user.cond(),
            user.expires_ms,
        )?;
        bookkeeping(&*self.db, vec![w]).await
    }

    /// Stores a newly verified `email` with a bookkeeping write: conditional on the user
    /// as read, a conflict ignored.
    pub async fn note_email(&self, user: &Versioned<User>, email: &str) -> Result<(), StateError> {
        let mut value = user.value.clone();
        value.email = Some(Sensitive::new(email.to_owned()));
        value.email_verified = true;
        let w = put(
            Some(&self.keys),
            keys::user(value.id),
            ONE.into(),
            &value,
            Some(user),
            user.cond(),
            user.expires_ms,
        )?;
        bookkeeping(&*self.db, vec![w]).await
    }

    /// Gives a user that has lost its personal store a new, empty one (named "Personal")
    /// and clears its deletion mark, in one write conditional on the user as read.
    pub async fn replace_personal_store(
        &self,
        user: &Versioned<User>,
        store: NewStore,
        now: i64,
    ) -> Result<Versioned<User>, StateError> {
        let mut value = user.value.clone();
        value.personal_store_id = store.id;
        value.deleted_at_ms = None;
        value.deleted_by = None;
        let record = personal_record(value.id, store, now);
        let w = put(
            Some(&self.keys),
            keys::user(value.id),
            ONE.into(),
            &value,
            Some(user),
            user.cond(),
            user.expires_ms,
        )?;
        let after = after_put(value, &w, Some(user.version));
        let mut writes = vec![w];
        writes.extend(store_writes(&self.keys, &record)?);
        self.db.write(writes).await?;
        Ok(after)
    }
}

fn personal_record(user: UserId, store: NewStore, now: i64) -> StoreRecord {
    StoreRecord::new(
        store.id,
        user,
        StoreKind::Personal,
        store.name.into_inner(),
        &store.wrapped,
        now,
    )
}
