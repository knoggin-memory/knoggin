// SPDX-License-Identifier: AGPL-3.0-only
//! Leases with their active-lease items, and container records. Server only. A lease
//! belongs to a slot: a user's own container, or one isolated or shared app's.
//!
//! A claim and "Mark gone" change a lease and a container in one write: build the parts
//! with `Leases::writes` and `Containers::write` and pass them to `StateDb::write` together.

use std::sync::Arc;

use uuid::Uuid;

use crate::db::{Cond, Item, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{Container, Lease, LeaseState, Slot, Versioned, decode_plain};
use crate::typed::{put, read, read_all};

/// Typed access to leases.
#[derive(Debug, Clone)]
pub struct Leases {
    db: Arc<dyn StateDb>,
}

impl Leases {
    /// Over `db`.
    pub fn new(db: Arc<dyn StateDb>) -> Self {
        Self { db }
    }

    /// The slot's lease; `None` means parked with generation 0 (`Lease::default()`).
    pub async fn get(&self, slot: Slot) -> Result<Option<Versioned<Lease>>, StateError> {
        read(&*self.db, None, &keys::lease(slot), ONE).await
    }

    /// The writes that store `lease` under `cond`: the lease, and its active-lease item,
    /// which is there exactly while the lease is not parked.
    pub fn writes(slot: Slot, lease: &Lease, cond: Cond) -> Result<Vec<Write>, StateError> {
        let active = if lease.state == LeaseState::Parked {
            Write::delete(keys::ACTIVE_LEASES, slot.to_string(), Cond::Any)
        } else {
            Write::put(
                Item::new(keys::ACTIVE_LEASES, slot.to_string(), serde_json::json!({})),
                Cond::Any,
            )
        };
        Ok(vec![
            put(None, keys::lease(slot), ONE.into(), lease, None, cond, None)?,
            active,
        ])
    }

    /// Stores `lease` alone under `cond`.
    pub async fn put(&self, slot: Slot, lease: &Lease, cond: Cond) -> Result<(), StateError> {
        self.db.write(Self::writes(slot, lease, cond)?).await
    }

    /// Up to `limit` slots whose lease is not parked, after `after`, in the order of
    /// their text.
    pub async fn list_active(
        &self,
        limit: usize,
        after: Option<Slot>,
    ) -> Result<Vec<Slot>, StateError> {
        let after = after.map(|a| a.to_string());
        self.db
            .query(keys::ACTIVE_LEASES, "", limit, after.as_deref())
            .await?
            .into_iter()
            .map(|i| {
                i.sk.parse()
                    .map_err(|_| StateError::Corrupt("an active-lease item"))
            })
            .collect()
    }
}

/// The deletes of a slot's lease and its active-lease item.
pub(crate) fn lease_deletes(slot: Slot) -> Vec<Write> {
    vec![
        Write::delete(keys::lease(slot), ONE, Cond::Any),
        Write::delete(keys::ACTIVE_LEASES, slot.to_string(), Cond::Any),
    ]
}

/// Typed access to container records.
#[derive(Debug, Clone)]
pub struct Containers {
    db: Arc<dyn StateDb>,
}

impl Containers {
    /// Over `db`.
    pub fn new(db: Arc<dyn StateDb>) -> Self {
        Self { db }
    }

    /// One container record.
    pub async fn get(&self, id: Uuid) -> Result<Option<Versioned<Container>>, StateError> {
        read(&*self.db, None, keys::CONTAINERS, &id.to_string()).await
    }

    /// Every container record.
    pub async fn list(&self) -> Result<Vec<Versioned<Container>>, StateError> {
        read_all(&*self.db, keys::CONTAINERS, "")
            .await?
            .into_iter()
            .map(decode_plain)
            .collect()
    }

    /// The write that stores `container` under `cond`.
    pub fn write(container: &Container, cond: Cond) -> Result<Write, StateError> {
        put(
            None,
            keys::CONTAINERS.into(),
            container.id.to_string(),
            container,
            None,
            cond,
            None,
        )
    }

    /// Stores `container` alone under `cond`.
    pub async fn put(&self, container: &Container, cond: Cond) -> Result<(), StateError> {
        self.db.write(vec![Self::write(container, cond)?]).await
    }

    /// Deletes a record (cleanup of `gone` ones).
    pub async fn delete(&self, id: Uuid, cond: Cond) -> Result<(), StateError> {
        self.db
            .write(vec![Write::delete(keys::CONTAINERS, id.to_string(), cond)])
            .await
    }
}
