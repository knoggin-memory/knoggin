// SPDX-License-Identifier: AGPL-3.0-only
//! Browser sessions and the per-user index of them.

use std::sync::Arc;

use knoggin_core::{Sensitive, UserId};

use crate::db::{Cond, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{Session, SessionIndex, Versioned, decode_plain};
use crate::seal::FieldKeys;
use crate::typed::{after_put, bookkeeping, put, read, read_all};

/// A user has at most this many sessions; a new one beyond it removes the oldest.
pub const MAX_SESSIONS: usize = 20;

/// `last_seen_ms` is written at most this often.
const SEEN_EVERY_MS: i64 = 60_000;

/// Typed access to browser sessions.
#[derive(Debug, Clone)]
pub struct Sessions {
    db: Arc<dyn StateDb>,
    keys: Arc<FieldKeys>,
}

impl Sessions {
    /// Over `db`, sealing with `keys`.
    pub fn new(db: Arc<dyn StateDb>, keys: Arc<FieldKeys>) -> Self {
        Self { db, keys }
    }

    /// Creates a session for the cookie value with SHA-256 `hash`, expiring `max_age_ms`
    /// after `now`, and then removes the user's oldest sessions beyond 20. `email` and
    /// `name` are set on a local install only.
    pub async fn create(
        &self,
        hash: &[u8; 32],
        user: UserId,
        now: i64,
        max_age_ms: i64,
        email: Option<Sensitive<String>>,
        name: Option<Sensitive<String>>,
    ) -> Result<Versioned<Session>, StateError> {
        let hash = keys::secret_hash(hash);
        let expires = Some(now.saturating_add(max_age_ms));
        let session = Session {
            user_id: user,
            created_ms: now,
            last_seen_ms: now,
            email,
            name,
        };
        let w = put(
            Some(&self.keys),
            keys::session(&hash),
            ONE.into(),
            &session,
            None,
            Cond::Absent,
            expires,
        )?;
        let after = after_put(session, &w, None);
        let index = put(
            None,
            keys::user_sessions(user),
            hash,
            &SessionIndex { created_ms: now },
            None,
            Cond::Absent,
            expires,
        )?;
        self.db.write(vec![w, index]).await?;
        let mut all = self.index(user).await?;
        if all.len() > MAX_SESSIONS {
            all.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
            let extra = all.len() - MAX_SESSIONS;
            self.delete_pairs(user, all.into_iter().take(extra).map(|(h, _)| h))
                .await?;
        }
        Ok(after)
    }

    /// The session of a cookie value's SHA-256.
    pub async fn get(&self, hash: &[u8; 32]) -> Result<Option<Versioned<Session>>, StateError> {
        read(
            &*self.db,
            Some(&self.keys),
            &keys::session(&keys::secret_hash(hash)),
            ONE,
        )
        .await
    }

    /// Writes `last_seen_ms` when it is more than a minute old, if the session is still as
    /// read; a conflict is ignored.
    pub async fn touch(
        &self,
        hash: &[u8; 32],
        session: &Versioned<Session>,
        now: i64,
    ) -> Result<(), StateError> {
        if now - session.value.last_seen_ms <= SEEN_EVERY_MS {
            return Ok(());
        }
        let mut value = session.value.clone();
        value.last_seen_ms = now;
        let w = put(
            Some(&self.keys),
            keys::session(&keys::secret_hash(hash)),
            ONE.into(),
            &value,
            Some(session),
            session.cond(),
            session.expires_ms,
        )?;
        bookkeeping(&*self.db, vec![w]).await
    }

    /// Ends one session (logout). A missing one is not an error.
    pub async fn delete(&self, hash: &[u8; 32]) -> Result<(), StateError> {
        let Some(s) = self.get(hash).await? else {
            return Ok(());
        };
        self.delete_pairs(s.value.user_id, [keys::secret_hash(hash)])
            .await
    }

    /// Ends every session of a user except `keep`; returns how many ended.
    pub async fn delete_all(
        &self,
        user: UserId,
        keep: Option<&[u8; 32]>,
    ) -> Result<usize, StateError> {
        let keep = keep.map(keys::secret_hash);
        let hashes: Vec<String> = self
            .index(user)
            .await?
            .into_iter()
            .map(|(h, _)| h)
            .filter(|h| Some(h) != keep.as_ref())
            .collect();
        let n = hashes.len();
        self.delete_pairs(user, hashes).await?;
        Ok(n)
    }

    /// The user's sessions as (hash, created_ms).
    async fn index(&self, user: UserId) -> Result<Vec<(String, i64)>, StateError> {
        read_all(&*self.db, &keys::user_sessions(user), "")
            .await?
            .into_iter()
            .map(|i| {
                let sk = i.sk.clone();
                decode_plain::<SessionIndex>(i).map(|v| (sk, v.value.created_ms))
            })
            .collect()
    }

    async fn delete_pairs(
        &self,
        user: UserId,
        hashes: impl IntoIterator<Item = String>,
    ) -> Result<(), StateError> {
        delete_session_pairs(&*self.db, user, hashes).await
    }
}

/// Deletes sessions with their index items, 12 pairs per write.
pub(crate) async fn delete_session_pairs(
    db: &dyn StateDb,
    user: UserId,
    hashes: impl IntoIterator<Item = String>,
) -> Result<(), StateError> {
    let writes: Vec<Write> = hashes
        .into_iter()
        .flat_map(|h| {
            [
                Write::delete(keys::session(&h), ONE, Cond::Any),
                Write::delete(keys::user_sessions(user), h, Cond::Any),
            ]
        })
        .collect();
    for chunk in writes.chunks(24) {
        db.write(chunk.to_vec()).await?;
    }
    Ok(())
}
