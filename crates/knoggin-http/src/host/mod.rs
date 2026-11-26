// SPDX-License-Identifier: AGPL-3.0-only
//! `StoreHost`: the open stores of a process and everything that runs around them.
//!
//! - Each open store sits behind one `std::sync::RwLock`, taken only inside blocking
//!   tasks. Writes pass a write gate (one permit) on the async side first, and every
//!   request takes a permit of the store's request semaphore (`try_acquire`).
//! - A panic inside a store operation, or a poisoned lock, drops that store from memory
//!   without saving it; the next request loads its last snapshot.
//! - Each open store has one task that runs its snapshot timers, its 24-hour sweep and
//!   its idle close ([`schedule`]). One worker task per process drains the embedding
//!   queues of all open stores through the [`Embedder`].
//!
//! The operations themselves (what REST and MCP call) are in [`ops`] and [`transfer`].

mod backend;
mod embedder;
mod local;
pub mod ops;
mod schedule;
pub mod transfer;

use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use aws_lc_rs::digest;
use knoggin_core::embed::ModelIdentity;
use knoggin_core::ops::{ChangeCounters, EmbeddingProgress, ModelState};
use knoggin_core::snapshot::{self, SnapshotError, SnapshotName};
use knoggin_core::store::request_hash_input;
use knoggin_core::{MemoryId, Store, StoreId, Timestamp};
use knoggin_crypto::container::{self, ContainerError};
use knoggin_crypto::{DataKey, WrappedKey};
use serde::Serialize;
use tokio::sync::{Notify, OnceCell, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

pub use backend::{BackendError, OpenedStore, SealedSnapshot, SnapshotSource, StoreBackend};
pub use embedder::{EmbedWaits, Embedder, Providers};
pub use local::LocalBackend;
pub use ops::Reply;

use crate::ctx::RequestContext;
use crate::error::{ApiError, Code, LockReason};
use crate::settings::StoreLimits;

/// The snapshot timing constants of one mode (tab "Snapshots and keys").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotTiming {
    /// Wait this long after the last content change.
    pub quiet: Duration,
    /// But never longer than this after the first unsaved content change.
    pub longest: Duration,
    /// Bookkeeping changes wait at most this long.
    pub bookkeeping: Duration,
}

impl SnapshotTiming {
    /// The local daemon.
    pub const LOCAL: Self = Self {
        quiet: Duration::from_millis(500),
        longest: Duration::from_secs(2),
        bookkeeping: Duration::from_secs(300),
    };
    /// A user container.
    pub const SERVER: Self = Self {
        quiet: Duration::from_secs(5),
        longest: Duration::from_secs(30),
        bookkeeping: Duration::from_secs(300),
    };
}

/// How often an open store is swept.
pub const SWEEP_EVERY: Duration = Duration::from_secs(24 * 3600);
/// A locked store is tried again at most this often.
pub const LOCK_RETRY: Duration = Duration::from_secs(60);
/// Snapshots tried when the newest ones cannot be read.
const MAX_SNAPSHOT_TRIES: usize = 3;
/// A store that answered `OverBudget` refuses writes with this `Retry-After`.
const UPLOAD_BLOCKED_RETRY: u32 = 60;

/// What a host is built with.
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Snapshot timing of the mode.
    pub timing: SnapshotTiming,
    /// The running model's identity: new stores take it, and an open store with another
    /// one adopts it.
    pub model: ModelIdentity,
    /// The `limits` settings: body limits, request timeout, requests per store.
    pub limits: StoreLimits,
    /// The memory budget of all open stores together, on the server; `None` locally.
    pub memory_budget: Option<u64>,
    /// Close a store after this long without a request (10 minutes locally,
    /// `idleSeconds` in a container); `None` keeps stores open.
    pub idle_close: Option<Duration>,
    /// How long after its last request a store with embedding work may stay open past
    /// `idle_close`: 30 minutes in a container, `None` (until the queue is empty) locally.
    pub embed_allowance: Option<Duration>,
    /// `webUrl` of `GET /v1/context`.
    pub web_url: Option<String>,
}

impl HostConfig {
    /// The local daemon's configuration for a model.
    pub fn local(model: ModelIdentity) -> Self {
        Self {
            timing: SnapshotTiming::LOCAL,
            model,
            limits: StoreLimits::default(),
            memory_budget: None,
            idle_close: Some(Duration::from_secs(600)),
            embed_allowance: None,
            web_url: None,
        }
    }
}

/// The current time for the engine.
pub(crate) fn now() -> Timestamp {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    Timestamp::from_millis(ms)
}

/// A new memory ID: a UUIDv7 from `at` (the request's `createdAt`, else now).
pub(crate) fn new_memory_id(at: Timestamp) -> MemoryId {
    let ms = u64::try_from(at.millis()).unwrap_or(0);
    let ts = uuid::Timestamp::from_unix(
        uuid::NoContext,
        ms / 1000,
        u32::try_from((ms % 1000) * 1_000_000).unwrap_or(0),
    );
    MemoryId::from_uuid(Uuid::new_v7(ts))
}

/// The request hash: SHA-256 of `request_hash_input(op, request)` (tab "Engine",
/// Idempotency), with aws-lc-rs.
pub fn request_hash<T: Serialize>(op: &str, request: &T) -> Result<[u8; 32], ApiError> {
    let input = request_hash_input(op, request)?;
    let d = digest::digest(&digest::SHA256, &input);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    Ok(out)
}

/// At most this many client names per open store.
const MAX_CLIENTS: usize = 32;

/// Notes `client` as seen `at`; a full list drops the name seen longest ago.
fn note_client(list: &mut Vec<(String, Timestamp)>, client: &str, at: Timestamp) {
    if let Some(entry) = list.iter_mut().find(|(name, _)| name == client) {
        entry.1 = at;
        return;
    }
    if list.len() >= MAX_CLIENTS
        && let Some(oldest) = list
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, seen))| *seen)
            .map(|(i, _)| i)
    {
        list.swap_remove(oldest);
    }
    list.push((client.to_owned(), at));
}

/// What a write does to the budget and the upload block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteKind {
    /// A client write: refused over the budget and while uploads are blocked.
    Client,
    /// A forget or a forced namespace delete: only makes room, so the budget lets it
    /// through.
    MakesRoom,
    /// Access recording, embedding results, the sweep: never refused, never a client write.
    Bookkeeping,
}

/// The timers of one store (rule 1 of "When snapshots are taken").
#[derive(Debug, Default)]
struct Timers {
    content_first: Option<Instant>,
    content_last: Option<Instant>,
    bookkeeping_first: Option<Instant>,
}

/// What the scheduler knows about saving one store.
#[derive(Debug)]
struct SaveState {
    next_sequence: u64,
    saved: ChangeCounters,
    restored: Option<SnapshotName>,
}

/// One open store.
pub(crate) struct OpenStore {
    id: StoreId,
    store: Arc<RwLock<Store>>,
    gate: Arc<Semaphore>,
    requests: Arc<Semaphore>,
    concurrency: usize,
    key: DataKey,
    wrapped: WrappedKey,
    generation: u64,
    save: tokio::sync::Mutex<SaveState>,
    /// Counters at the last finished snapshot, readable without the save lock.
    saved: Mutex<ChangeCounters>,
    current: Mutex<ChangeCounters>,
    timers: Mutex<Timers>,
    wake: Notify,
    estimated: AtomicU64,
    memory_count: AtomicU64,
    pending: AtomicU64,
    embeddable: AtomicU64,
    last_sequence: AtomicU64,
    full: AtomicBool,
    read_only: AtomicBool,
    upload_blocked: AtomicBool,
    dropped: AtomicBool,
    last_request: Mutex<Instant>,
    /// The MCP client names seen, with when each was last seen; in memory only.
    clients: Mutex<Vec<(String, Timestamp)>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for OpenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenStore")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl OpenStore {
    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn unsaved(&self) -> bool {
        *Self::lock(&self.current) != *Self::lock(&self.saved)
    }

    /// Notes what one operation changed and arms the timers for it.
    fn note(&self, before: ChangeCounters, after: ChangeCounters, store: &Store) {
        *Self::lock(&self.current) = after;
        self.estimated
            .store(store.estimated_bytes(), Ordering::Relaxed);
        let summary = store.summary();
        self.memory_count
            .store(summary.memory_count, Ordering::Relaxed);
        self.pending
            .store(summary.pending_embeddings, Ordering::Relaxed);
        self.embeddable.store(summary.embeddable, Ordering::Relaxed);
        let now = Instant::now();
        let mut t = Self::lock(&self.timers);
        let mut armed = false;
        if after.content != before.content {
            t.content_first.get_or_insert(now);
            t.content_last = Some(now);
            armed = true;
        }
        if after.bookkeeping != before.bookkeeping {
            t.bookkeeping_first.get_or_insert(now);
            armed = true;
        }
        drop(t);
        if armed {
            self.wake.notify_one();
        }
    }
}

/// One store's entry while it opens or is open.
#[derive(Debug, Default)]
struct Entry {
    cell: OnceCell<Arc<OpenStore>>,
}

/// Running totals since the process started, for the container's `status` answer.
#[derive(Debug, Default)]
pub struct HostCounters {
    /// Store requests.
    pub requests: AtomicU64,
    /// Texts embedded by the queue.
    pub embed_count: AtomicU64,
    /// Time spent on them.
    pub embed_millis: AtomicU64,
    /// Snapshots written.
    pub snapshot_count: AtomicU64,
    /// Time spent writing them.
    pub snapshot_millis: AtomicU64,
    /// Bytes written.
    pub snapshot_bytes: AtomicU64,
    /// Snapshot writes that failed.
    pub snapshot_failures: AtomicU64,
}

/// The state of one store, for the status routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreState {
    /// Not in memory; the next request loads it.
    Parked,
    /// Being loaded.
    Waking,
    /// Open in memory.
    Ready,
    /// Cannot be opened.
    Locked(LockReason),
}

/// The status route's figures of one store.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreStatus {
    /// Where the store stands.
    pub state: StoreState,
    /// Refused a write for the budget, or served for reading only.
    pub full: bool,
    /// `{ pending, total }` for an open store.
    pub embedding: Option<EmbeddingProgress>,
}

/// One open store in the container's `status` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStoreReport {
    /// The store.
    pub store_id: StoreId,
    /// Memories in every phase.
    pub memory_count: u64,
    /// The engine's size estimate.
    pub estimated_bytes: u64,
    /// Has changes no snapshot holds yet.
    pub unsaved: bool,
    /// The sequence of the last snapshot written in this open, 0 before the first.
    pub last_sequence: u64,
    /// Memories waiting in the embedding queue.
    pub pending_embeddings: u64,
    /// Memories that can hold a vector.
    pub total_embeddable: u64,
    /// As in `StoreStatus`.
    pub full: bool,
}

/// The open stores and their locks, for the container's `status` answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostReport {
    /// Open stores.
    pub stores: Vec<OpenStoreReport>,
    /// Stores being loaded.
    pub opening: Vec<StoreId>,
    /// Stores that failed to open.
    pub locks: Vec<(StoreId, LockReason)>,
}

/// The open stores of one process, and the embedding worker, snapshot scheduler and
/// sweep timer around them. Built once per process (per claim in a container).
pub struct StoreHost {
    me: Weak<StoreHost>,
    config: HostConfig,
    backend: Arc<dyn StoreBackend>,
    embedder: Embedder,
    entries: Mutex<HashMap<StoreId, Arc<Entry>>>,
    locks: Mutex<HashMap<StoreId, (LockReason, Instant)>>,
    work: Notify,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    closing: AtomicBool,
    lease_lost: watch::Sender<bool>,
    /// Running totals.
    pub counters: HostCounters,
}

impl fmt::Debug for StoreHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreHost")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// Why loading one snapshot failed.
enum LoadError {
    /// Not readable under its key, or its content does not decode: try an older one.
    Corrupt,
    /// The content format is newer than this build reads.
    FormatTooNew,
    /// The content is larger than the whole budget.
    TooLarge,
    /// The bytes did not all arrive: try again later.
    Transit,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl StoreHost {
    /// Builds the host and starts the embedding worker. Call inside a Tokio runtime.
    pub fn new(
        config: HostConfig,
        backend: Arc<dyn StoreBackend>,
        embedder: Embedder,
    ) -> Arc<Self> {
        let host = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            config,
            backend,
            embedder,
            entries: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            work: Notify::new(),
            tasks: Mutex::new(Vec::new()),
            closing: AtomicBool::new(false),
            lease_lost: watch::Sender::new(false),
            counters: HostCounters::default(),
        });
        let worker = tokio::spawn(schedule::embed_worker(Arc::downgrade(&host)));
        lock(&host.tasks).push(worker);
        host
    }

    /// The host's configuration.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// The Model state of this process.
    pub fn model_state(&self) -> ModelState {
        self.embedder.state()
    }

    /// Becomes true once a backend answered `lease_lost`: the container then stops
    /// serving at once and exits without writing.
    pub fn lease_lost(&self) -> watch::Receiver<bool> {
        self.lease_lost.subscribe()
    }

    /// The open store of the request's store, opening it on first use.
    async fn open_for(
        &self,
        ctx: &RequestContext,
    ) -> Result<(Arc<OpenStore>, OwnedSemaphorePermit), ApiError> {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        let open = self.open_store(ctx.store_id).await?;
        let permit = open
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::unavailable(1))?;
        *lock(&open.last_request) = Instant::now();
        if let Some(client) = &ctx.client {
            note_client(&mut lock(&open.clients), client, now());
        }
        Ok((open, permit))
    }

    /// The MCP client names an open store has seen, most recent first, with when each
    /// was last seen (for the web UI's system status). Empty for a store not open.
    pub fn clients(&self, id: StoreId) -> Vec<(String, Timestamp)> {
        let open = lock(&self.entries)
            .get(&id)
            .and_then(|e| e.cell.get().cloned());
        let mut list = open.map(|o| lock(&o.clients).clone()).unwrap_or_default();
        list.sort_by_key(|(_, seen)| std::cmp::Reverse(*seen));
        list
    }

    async fn open_store(&self, id: StoreId) -> Result<Arc<OpenStore>, ApiError> {
        if self.closing.load(Ordering::Relaxed) {
            return Err(ApiError::unavailable(2));
        }
        if let Some((reason, at)) = lock(&self.locks).get(&id).copied()
            && at.elapsed() < LOCK_RETRY
        {
            return Err(ApiError::store_locked(reason));
        }
        let entry = lock(&self.entries).entry(id).or_default().clone();
        let result = entry.cell.get_or_try_init(|| self.load(id)).await.cloned();
        match result {
            Ok(open) => Ok(open),
            Err(e) => {
                let mut entries = lock(&self.entries);
                if entries
                    .get(&id)
                    .is_some_and(|e| Arc::ptr_eq(e, &entry) && e.cell.get().is_none())
                {
                    entries.remove(&id);
                }
                drop(entries);
                if e.code == Code::StoreLocked
                    && let Some(reason) = lock_reason_of(&e)
                {
                    lock(&self.locks).insert(id, (reason, Instant::now()));
                }
                Err(e)
            }
        }
    }

    /// Loading a store (tab "Snapshots and keys", "Loading a store").
    async fn load(&self, id: StoreId) -> Result<Arc<OpenStore>, ApiError> {
        if let Some((reason, at)) = lock(&self.locks).get(&id).copied()
            && at.elapsed() < LOCK_RETRY
        {
            return Err(ApiError::store_locked(reason));
        }
        let mut after = None;
        let mut tries = 0;
        loop {
            let opened = self
                .backend
                .open(id, after)
                .await
                .map_err(|e| self.backend_error(id, e))?;
            let OpenedStore {
                key,
                wrapped,
                generation,
                next_sequence,
                restore,
                snapshot,
            } = opened;
            let name = snapshot.as_ref().map(|s| s.name);
            let model = self.config.model.clone();
            let max_len = self.config.memory_budget.unwrap_or(u64::MAX);
            let read_key = key.clone();
            let loaded = tokio::task::spawn_blocking(move || {
                let now = now();
                let mut store = match snapshot {
                    None => Store::create(id, model.clone(), now),
                    Some(source) => read_snapshot(id, source, &read_key, max_len)?,
                };
                let saved = store.changes();
                store.on_open(now);
                if *store.model() != model {
                    store.adopt_model(model);
                }
                store.sweep(now);
                Ok::<_, LoadError>((store, saved))
            })
            .await
            .map_err(|e| ApiError::from_join(&e))?;
            match loaded {
                Ok((store, saved)) => {
                    let restored = if restore { name } else { None };
                    return Ok(self.install(
                        id,
                        store,
                        saved,
                        key,
                        wrapped,
                        generation,
                        next_sequence,
                        restored,
                    ));
                }
                Err(LoadError::FormatTooNew) => {
                    return Err(ApiError::store_locked(LockReason::FormatTooNew));
                }
                Err(LoadError::TooLarge) => {
                    return Err(ApiError::store_locked(LockReason::OutOfMemory));
                }
                Err(LoadError::Transit) => return Err(ApiError::unavailable(2)),
                Err(LoadError::Corrupt) => {
                    tracing::warn!(
                        store_id = %id,
                        snapshot = %name.map(|n| n.to_string()).unwrap_or_default(),
                        kind = "corrupt",
                        "a snapshot could not be read"
                    );
                    metrics::counter!("knoggin_snapshot_failures_total").increment(1);
                    tries += 1;
                    if restore || tries >= MAX_SNAPSHOT_TRIES {
                        return Err(ApiError::store_locked(LockReason::SnapshotUnreadable));
                    }
                    after = name;
                }
            }
        }
    }

    #[expect(clippy::too_many_arguments, reason = "the parts of one opened store")]
    fn install(
        &self,
        id: StoreId,
        store: Store,
        saved: ChangeCounters,
        key: DataKey,
        wrapped: WrappedKey,
        generation: u64,
        next_sequence: u64,
        restored: Option<SnapshotName>,
    ) -> Arc<OpenStore> {
        let summary = store.summary();
        let estimated = store.estimated_bytes();
        let current = store.changes();
        let open = Arc::new(OpenStore {
            id,
            store: Arc::new(RwLock::new(store)),
            gate: Arc::new(Semaphore::new(1)),
            requests: Arc::new(Semaphore::new(self.config.limits.store_concurrency)),
            concurrency: self.config.limits.store_concurrency,
            key,
            wrapped,
            generation,
            save: tokio::sync::Mutex::new(SaveState {
                next_sequence,
                saved,
                restored,
            }),
            saved: Mutex::new(saved),
            current: Mutex::new(current),
            timers: Mutex::new(Timers::default()),
            wake: Notify::new(),
            estimated: AtomicU64::new(estimated),
            memory_count: AtomicU64::new(summary.memory_count),
            pending: AtomicU64::new(summary.pending_embeddings),
            embeddable: AtomicU64::new(summary.embeddable),
            last_sequence: AtomicU64::new(0),
            full: AtomicBool::new(false),
            read_only: AtomicBool::new(false),
            upload_blocked: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            last_request: Mutex::new(Instant::now()),
            clients: Mutex::new(Vec::new()),
            task: Mutex::new(None),
        });
        if current.bookkeeping != saved.bookkeeping {
            lock(&open.timers).bookkeeping_first = Some(Instant::now());
        }
        // Step 7 of "Loading a store": over what is left of the budget, read only.
        if let Some(budget) = self.config.memory_budget
            && self.total_estimated().saturating_add(estimated) > budget
        {
            open.read_only.store(true, Ordering::Relaxed);
        }
        lock(&self.locks).remove(&id);
        let handle = tokio::spawn(schedule::store_task(self.me.clone(), open.clone()));
        *lock(&open.task) = Some(handle);
        metrics::gauge!("knoggin_open_stores").increment(1.0);
        if summary.pending_embeddings > 0 {
            self.work.notify_one();
        }
        open
    }

    fn backend_error(&self, id: StoreId, e: BackendError) -> ApiError {
        match e {
            BackendError::Locked(reason) => ApiError::store_locked(reason),
            BackendError::Deleted => ApiError::store_deleting(),
            BackendError::LeaseLost => {
                self.on_lease_lost();
                ApiError::unavailable(2)
            }
            BackendError::OverBudget => ApiError::new(
                Code::RateLimited,
                "Too many snapshot uploads; try again later",
            )
            .with_retry_after(UPLOAD_BLOCKED_RETRY),
            BackendError::NameTaken | BackendError::Unavailable => {
                tracing::warn!(store_id = %id, kind = e.kind(), "opening a store failed");
                ApiError::unavailable(2)
            }
        }
    }

    /// The engine's estimate summed over every open store.
    fn total_estimated(&self) -> u64 {
        self.open_list()
            .iter()
            .map(|o| o.estimated.load(Ordering::Relaxed))
            .fold(0, u64::saturating_add)
    }

    /// The open stores, by ID.
    fn open_list(&self) -> Vec<Arc<OpenStore>> {
        let mut list: Vec<Arc<OpenStore>> = lock(&self.entries)
            .values()
            .filter_map(|e| e.cell.get().cloned())
            .collect();
        list.sort_by_key(|o| o.id);
        list
    }

    /// Refuses a client write over the budget, on a read-only store, or while uploads
    /// are blocked.
    fn admit(&self, open: &OpenStore, kind: WriteKind) -> Result<(), ApiError> {
        if kind == WriteKind::Bookkeeping {
            return Ok(());
        }
        if open.upload_blocked.load(Ordering::Relaxed) {
            return Err(ApiError::new(
                Code::RateLimited,
                "The store takes no writes until its changes have been saved; try again later",
            )
            .with_retry_after(UPLOAD_BLOCKED_RETRY));
        }
        if kind == WriteKind::Client
            && let Some(budget) = self.config.memory_budget
            && (open.read_only.load(Ordering::Relaxed) || self.total_estimated() >= budget)
        {
            open.full.store(true, Ordering::Relaxed);
            return Err(ApiError::resource_exhausted());
        }
        Ok(())
    }

    /// After the total fell: a read-only store that now fits takes writes again.
    fn recheck_budget(&self) {
        let Some(budget) = self.config.memory_budget else {
            return;
        };
        if self.total_estimated() < budget {
            for open in self.open_list() {
                open.read_only.store(false, Ordering::Relaxed);
            }
        }
    }

    /// Runs `f` under the read lock in a blocking task.
    pub(crate) async fn read_on<R: Send + 'static>(
        &self,
        open: &Arc<OpenStore>,
        f: impl FnOnce(&Store) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        let store = open.store.clone();
        let out = tokio::task::spawn_blocking(move || {
            let guard = store.read().map_err(|_| Poisoned)?;
            Ok::<_, Poisoned>(f(&guard))
        })
        .await;
        self.finish(open, out)?
    }

    /// Runs `f` under the write lock in a blocking task, after the write gate, and arms
    /// the snapshot timers for what it changed.
    pub(crate) async fn write_on<R: Send + 'static>(
        &self,
        open: &Arc<OpenStore>,
        kind: WriteKind,
        f: impl FnOnce(&mut Store) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        self.admit(open, kind)?;
        let gate = open
            .gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::unavailable(1))?;
        let store = open.store.clone();
        let watcher = open.clone();
        let out = tokio::task::spawn_blocking(move || {
            // The gate is released only when the blocking work ends, even if the request
            // that started it has gone.
            let _gate = gate;
            let mut guard = store.write().map_err(|_| Poisoned)?;
            let before = guard.changes();
            let result = f(&mut guard);
            watcher.note(before, guard.changes(), &guard);
            Ok::<_, Poisoned>(result)
        })
        .await;
        let result = self.finish(open, out)?;
        if result.is_ok() && kind != WriteKind::Bookkeeping {
            open.full.store(false, Ordering::Relaxed);
        }
        if kind == WriteKind::MakesRoom {
            self.recheck_budget();
        }
        result
    }

    fn finish<R>(
        &self,
        open: &Arc<OpenStore>,
        out: Result<Result<Result<R, ApiError>, Poisoned>, tokio::task::JoinError>,
    ) -> Result<Result<R, ApiError>, ApiError> {
        match out {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(Poisoned)) => {
                tracing::error!(store_id = %open.id, kind = "poisoned", "a store lock was poisoned");
                self.drop_open(open);
                Err(ApiError::internal())
            }
            Err(join) => {
                let e = ApiError::from_join(&join);
                if join.is_panic() {
                    // Never save a store that was half changed.
                    self.drop_open(open);
                }
                Err(e)
            }
        }
    }

    /// Drops a store from memory without saving it: deleted, poisoned or half changed.
    /// The next request loads its last snapshot.
    pub fn drop_store(&self, id: StoreId) {
        // The map's guard must end before `drop_open` takes the map again.
        let open = lock(&self.entries)
            .get(&id)
            .and_then(|e| e.cell.get().cloned());
        if let Some(open) = open {
            self.drop_open(&open);
        }
    }

    fn drop_open(&self, open: &Arc<OpenStore>) {
        if open.dropped.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut entries = lock(&self.entries);
            if entries
                .get(&open.id)
                .and_then(|e| e.cell.get())
                .is_some_and(|o| Arc::ptr_eq(o, open))
            {
                entries.remove(&open.id);
            }
        }
        if let Some(task) = lock(&open.task).take() {
            task.abort();
        }
        metrics::gauge!("knoggin_open_stores").decrement(1.0);
        self.recheck_budget();
    }

    fn on_lease_lost(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.lease_lost.send_replace(true);
        for open in self.open_list() {
            self.drop_open(&open);
        }
    }

    /// Writes a final snapshot of one store if it has unsaved changes, then drops it.
    /// A store whose save fails stays open.
    pub async fn close(&self, id: StoreId) -> Result<(), BackendError> {
        let Some(open) = lock(&self.entries)
            .get(&id)
            .and_then(|e| e.cell.get().cloned())
        else {
            return Ok(());
        };
        self.close_open(&open).await
    }

    async fn close_open(&self, open: &Arc<OpenStore>) -> Result<(), BackendError> {
        schedule::snapshot(self, open).await?;
        self.drop_open(open);
        Ok(())
    }

    /// One final snapshot of every open store with unsaved changes of either kind (park,
    /// shutdown, a clean exit). Returns the stores whose save failed; the role decides how
    /// often to try again. The embedding queue is not waited for.
    pub async fn save_all(&self) -> Vec<(StoreId, BackendError)> {
        let mut failed = Vec::new();
        for open in self.open_list() {
            if let Err(e) = schedule::snapshot(self, &open).await {
                failed.push((open.id, e));
            }
        }
        failed
    }

    /// Stops taking requests and stops every task the host started. Call `save_all`
    /// first.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
        for open in self.open_list() {
            if let Some(task) = lock(&open.task).take() {
                task.abort();
            }
        }
    }

    /// The status of one store. A locked store whose last try is more than a minute old
    /// is tried again in the background; this call is not activity.
    pub fn store_status(&self, id: StoreId) -> StoreStatus {
        let entry = lock(&self.entries).get(&id).cloned();
        if let Some(entry) = entry {
            return match entry.cell.get() {
                Some(open) => StoreStatus {
                    state: StoreState::Ready,
                    full: open.full.load(Ordering::Relaxed)
                        || open.read_only.load(Ordering::Relaxed),
                    embedding: Some(EmbeddingProgress {
                        pending: open.pending.load(Ordering::Relaxed),
                        total: open.embeddable.load(Ordering::Relaxed),
                    }),
                },
                None => StoreStatus {
                    state: StoreState::Waking,
                    full: false,
                    embedding: None,
                },
            };
        }
        let locked = lock(&self.locks).get(&id).copied();
        let state = match locked {
            Some((reason, at)) => {
                if at.elapsed() >= LOCK_RETRY
                    && let Some(host) = self.me.upgrade()
                {
                    let retry = tokio::spawn(async move {
                        let _ = host.open_store(id).await;
                    });
                    let mut tasks = lock(&self.tasks);
                    tasks.retain(|t| !t.is_finished());
                    tasks.push(retry);
                }
                StoreState::Locked(reason)
            }
            None => StoreState::Parked,
        };
        StoreStatus {
            state,
            full: false,
            embedding: None,
        }
    }

    /// The open stores, those being loaded and the locks.
    pub fn report(&self) -> HostReport {
        let mut report = HostReport::default();
        for entry in lock(&self.entries).iter() {
            match entry.1.cell.get() {
                Some(open) => report.stores.push(OpenStoreReport {
                    store_id: open.id,
                    memory_count: open.memory_count.load(Ordering::Relaxed),
                    estimated_bytes: open.estimated.load(Ordering::Relaxed),
                    unsaved: open.unsaved(),
                    last_sequence: open.last_sequence.load(Ordering::Relaxed),
                    pending_embeddings: open.pending.load(Ordering::Relaxed),
                    total_embeddable: open.embeddable.load(Ordering::Relaxed),
                    full: open.full.load(Ordering::Relaxed)
                        || open.read_only.load(Ordering::Relaxed),
                }),
                None => report.opening.push(*entry.0),
            }
        }
        report.locks = lock(&self.locks)
            .iter()
            .map(|(id, (r, _))| (*id, *r))
            .collect();
        report.stores.sort_by_key(|s| s.store_id);
        report.opening.sort();
        report.locks.sort_by_key(|l| l.0);
        report
    }

    /// Tests only: runs `f` on a store as a client write does.
    #[doc(hidden)]
    pub async fn write_for_test<R: Send + 'static>(
        &self,
        ctx: &RequestContext,
        f: impl FnOnce(&mut Store) -> R + Send + 'static,
    ) -> Result<R, ApiError> {
        let (open, _permit) = self.open_for(ctx).await?;
        self.write_on(&open, WriteKind::Client, move |s| Ok(f(s)))
            .await
    }

    /// Whether a store is open in memory.
    pub fn is_open(&self, id: StoreId) -> bool {
        lock(&self.entries)
            .get(&id)
            .is_some_and(|e| e.cell.get().is_some())
    }

    pub(crate) fn notify_work(&self) {
        self.work.notify_one();
    }
}

/// A store lock was poisoned by a panic.
struct Poisoned;

fn lock_reason_of(e: &ApiError) -> Option<LockReason> {
    [
        LockReason::MasterKeyMissing,
        LockReason::SnapshotUnreadable,
        LockReason::FormatTooNew,
        LockReason::OutOfMemory,
        LockReason::StateUnreadable,
        LockReason::StateMissing,
    ]
    .into_iter()
    .find(|r| Some(r.as_str()) == e.reason)
}

/// Reads, checks, decrypts and decodes one snapshot, on a blocking thread.
fn read_snapshot(
    id: StoreId,
    source: SnapshotSource,
    key: &DataKey,
    max_len: u64,
) -> Result<Store, LoadError> {
    let SnapshotSource { name, mut reader } = source;
    let from_container = |e: ContainerError| match e {
        ContainerError::Io(_) => LoadError::Transit,
        ContainerError::TooLarge { .. } => LoadError::TooLarge,
        _ => LoadError::Corrupt,
    };
    let header = container::read_header_from(&mut reader).map_err(from_container)?;
    header.expect(id, name).map_err(from_container)?;
    let content = container::open(&header, key, &mut reader as &mut dyn Read, max_len)
        .map_err(from_container)?;
    // The header is trusted only now that a chunk has decrypted under it.
    snapshot::check_content_format(header.content_format)
        .and_then(|()| Store::decode(header.content_format, &content))
        .map_err(|e| match e {
            SnapshotError::FormatTooNew(_) => LoadError::FormatTooNew,
            SnapshotError::Corrupt(_) => LoadError::Corrupt,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_names_keep_the_32_seen_last() {
        let mut list = Vec::new();
        for i in 0..32 {
            note_client(&mut list, &format!("c{i}"), Timestamp::from_millis(i));
        }
        note_client(&mut list, "c0", Timestamp::from_millis(100));
        note_client(&mut list, "new", Timestamp::from_millis(101));
        assert_eq!(list.len(), MAX_CLIENTS);
        assert!(
            !list.iter().any(|(n, _)| n == "c1"),
            "the one seen longest ago goes"
        );
        assert!(list.iter().any(|(n, at)| n == "c0" && at.millis() == 100));
    }
}
