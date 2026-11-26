// SPDX-License-Identifier: AGPL-3.0-only
//! `StoreBackend`: where an open store gets its key and its snapshots. `LocalBackend`
//! (files and the local key provider) and the server's `RouterBackend` implement it.

use std::fmt;
use std::io::Read;

use async_trait::async_trait;
use knoggin_core::StoreId;
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::time::Timestamp;
use knoggin_crypto::container::Sealer;
use knoggin_crypto::{DataKey, WrappedKey};

use crate::error::LockReason;

/// Opens and saves stores. Used as `Arc<dyn StoreBackend>`.
#[async_trait]
pub trait StoreBackend: Send + Sync + fmt::Debug {
    /// The data key, the generation, the next sequence and the snapshot to load. With
    /// `after`, the newest snapshot below that name, because the newest one could not be
    /// read. Never a snapshot below the one the store's record names as newest.
    async fn open(
        &self,
        store: StoreId,
        after: Option<SnapshotName>,
    ) -> Result<OpenedStore, BackendError>;

    /// Writes one sealed snapshot under its name; never overwrites one.
    async fn save(&self, store: StoreId, snapshot: SealedSnapshot) -> Result<(), BackendError>;
}

/// What `open` hands the host.
pub struct OpenedStore {
    /// The store's data key.
    pub key: DataKey,
    /// Its wrapped form; every snapshot header carries it and its `kek_id`.
    pub wrapped: WrappedKey,
    /// The generation of this open: lease generation on the server, open counter locally.
    pub generation: u64,
    /// The first sequence to write.
    pub next_sequence: u64,
    /// The snapshot is one an admin chose to restore: no fall-back on a failed read.
    pub restore: bool,
    /// The snapshot to load, or none for a new store.
    pub snapshot: Option<SnapshotSource>,
}

impl fmt::Debug for OpenedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedStore")
            .field("generation", &self.generation)
            .field("next_sequence", &self.next_sequence)
            .field("restore", &self.restore)
            .field("snapshot", &self.snapshot.as_ref().map(|s| s.name))
            .finish_non_exhaustive()
    }
}

/// A snapshot to load: its name and a reader over its sealed bytes. The host reads it
/// inside a blocking task, so a reader that bridges an async download is built lazily.
pub struct SnapshotSource {
    /// Its name.
    pub name: SnapshotName,
    /// The sealed file, header first.
    pub reader: Box<dyn Read + Send>,
}

impl fmt::Debug for SnapshotSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSource")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// One snapshot to write: its name, its sealed bytes as chunks, and the figures that the
/// commit records.
pub struct SealedSnapshot {
    /// The name, which the header carries too.
    pub name: SnapshotName,
    /// The header, then each sealed chunk. Sealing happens as the chunks are taken, so
    /// consume it on a blocking thread.
    pub chunks: Sealer,
    /// The whole file's length (`container::sealed_len`).
    pub len: u64,
    /// Memories in every phase.
    pub memory_count: u64,
    /// Namespaces.
    pub namespace_count: u64,
    /// The last client write.
    pub last_write_at: Option<Timestamp>,
    /// The client of the last client write. Sensitive.
    pub last_write_client: Option<String>,
    /// The restored snapshot's name, until a save after the restore has gone through.
    pub restored: Option<SnapshotName>,
}

impl fmt::Debug for SealedSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedSnapshot")
            .field("name", &self.name)
            .field("len", &self.len)
            .field("memory_count", &self.memory_count)
            .field("namespace_count", &self.namespace_count)
            .finish_non_exhaustive()
    }
}

/// Why a backend call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    /// That sequence exists. Take the next.
    #[error("the snapshot name is taken")]
    NameTaken,
    /// The lease is gone: stop serving at once and write nothing more.
    #[error("the lease was lost")]
    LeaseLost,
    /// The store is deleted: drop it without saving.
    #[error("the store is deleted")]
    Deleted,
    /// The hourly upload budget is used up: keep the changes, take no writes until an
    /// upload goes through.
    #[error("the upload budget is used up")]
    OverBudget,
    /// The store cannot be opened.
    #[error("the store is locked")]
    Locked(LockReason),
    /// Try again later.
    #[error("the backend is unavailable")]
    Unavailable,
    /// A bug: the backend's own work panicked. Answered as `internal`.
    #[error("the backend failed")]
    Internal,
}

impl BackendError {
    /// A short fixed word for logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NameTaken => "name_taken",
            Self::LeaseLost => "lease_lost",
            Self::Deleted => "deleted",
            Self::OverBudget => "over_budget",
            Self::Locked(_) => "locked",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}
