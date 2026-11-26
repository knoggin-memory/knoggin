// SPDX-License-Identifier: AGPL-3.0-only
//! `StoreHost`: the open stores of a process and everything that runs around them.
//!
//! - Each open store sits behind one `std::sync::RwLock`, taken only inside blocking
//!   tasks. Writes pass a write gate (one permit) on the async side first, and every
//!   request takes a permit of the store's request semaphore (`try_acquire`).
//! - A panic inside a store operation, or a poisoned lock, drops that store from memory
//!   without saving it; the next request loads its last snapshot.
//! - Each open store has one task that runs its snapshot timers, its 24-hour sweep and
//!   its idle close (`schedule`). One worker task per process drains the embedding
//!   queues of all open stores through the [`Embedder`].
//!
//! The operations themselves (what REST and MCP call) are in [`ops`] and [`transfer`].

mod backend;
mod embedder;
pub(crate) mod live;
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
use knoggin_core::ops::{ChangeCounters, EmbeddingProgress, ModelState, StoreSummary};
use knoggin_core::snapshot::{self, SnapshotError, SnapshotName};
use knoggin_core::store::request_hash_input;
use knoggin_core::{MemoryId, Store, StoreId, Timestamp};
use knoggin_crypto::container::{self, ContainerError};
use knoggin_crypto::{DataKey, WrappedKey};
use serde::Serialize;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
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
    /// `webUrl` of `GET /v1/context`, and the address the MCP instructions name.
    pub web_url: Option<String>,
    /// The MCP endpoint's settings.
    pub mcp: crate::mcp::McpConfig,
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
            mcp: crate::mcp::McpConfig::default(),
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

/// Notes `client` as seen `at`. The list is kept oldest first, so ties in `at` keep the
/// order of arrival; a full list drops the name seen longest ago.
fn note_client(list: &mut Vec<(String, Timestamp)>, client: &str, at: Timestamp) {
    list.retain(|(name, _)| name != client);
    if list.len() >= MAX_CLIENTS {
        list.remove(0);
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
    concurrency: u32,
    /// True while the store is being closed: new requests wait for the close to end.
    closing: watch::Sender<bool>,
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
    /// The web UI's live events.
    events: live::EventLog,
    /// When the store was last swept: at open, then every 24 hours.
    last_sweep: Mutex<Timestamp>,
    /// The last snapshot attempt failed.
    snapshot_failed: AtomicBool,
    /// The size of the last snapshot read or written, when known.
    snapshot_bytes: Mutex<Option<u64>>,
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
    /// The store's live events.
    pub(crate) fn events(&self) -> &live::EventLog {
        &self.events
    }

    /// When the store was last swept.
    pub(crate) fn last_sweep(&self) -> Timestamp {
        *lock(&self.last_sweep)
    }

    /// The size of the last snapshot read or written in this open, when known.
    pub(crate) fn snapshot_bytes(&self) -> Option<u64> {
        *lock(&self.snapshot_bytes)
    }

    /// Why the store's subsystem is degraded: its last snapshot failed, or it takes no
    /// writes. `None` when it is well.
    pub(crate) fn degraded(&self) -> Option<&'static str> {
        if self.snapshot_failed.load(Ordering::Relaxed) {
            Some("The last snapshot failed")
        } else if self.read_only.load(Ordering::Relaxed)
            || self.upload_blocked.load(Ordering::Relaxed)
        {
            Some("The store takes no writes")
        } else {
            None
        }
    }

    fn unsaved(&self) -> bool {
        *lock(&self.current) != *lock(&self.saved)
    }

    /// Refused a write for the budget, or served for reading only.
    fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed) || self.read_only.load(Ordering::Relaxed)
    }

    /// Notes what one operation changed and arms the timers for it.
    fn note(&self, before: ChangeCounters, after: ChangeCounters, store: &Store) {
        *lock(&self.current) = after;
        self.estimated
            .store(store.estimated_bytes(), Ordering::Relaxed);
        let summary = store.summary();
        self.memory_count
            .store(summary.memory_count, Ordering::Relaxed);
        self.pending
            .store(summary.pending_embeddings, Ordering::Relaxed);
        self.embeddable.store(summary.embeddable, Ordering::Relaxed);
        let now = Instant::now();
        let mut t = lock(&self.timers);
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

/// What a store's load ended with; `None` while it runs.
type LoadState = Option<Result<Arc<OpenStore>, ApiError>>;

/// One store's entry while it opens or is open. The load runs as a task of its own that
/// every caller waits on, so a caller that gives up (its request timed out) cancels
/// nothing and a second caller starts no second load.
#[derive(Debug)]
struct Entry {
    state: watch::Receiver<LoadState>,
}

impl Entry {
    /// The open store, once the load has succeeded.
    fn open(&self) -> Option<Arc<OpenStore>> {
        self.state.borrow().as_ref()?.as_ref().ok().cloned()
    }
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
    /// How often the embedding worker went to wait for work; a test watches it.
    idle_waits: AtomicU64,
    /// Running totals.
    pub counters: HostCounters,
    /// When the host started, for `GET /v1/ui/system`.
    started_at: Timestamp,
    started: std::time::Instant,
    /// The last 100 recalls and document embeddings.
    pub(crate) recall_ms: live::Latencies,
    pub(crate) embed_ms: live::Latencies,
}

impl fmt::Debug for StoreHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreHost")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A loaded store with the figures `install` needs, all taken on the blocking thread.
struct Decoded {
    store: Store,
    /// The counters as the snapshot left them.
    saved: ChangeCounters,
    /// The counters after the open's own changes (open, model change, sweep).
    current: ChangeCounters,
    summary: StoreSummary,
    estimated: u64,
    /// The size of the snapshot it was read from.
    snapshot_bytes: Option<u64>,
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
            idle_waits: AtomicU64::new(0),
            counters: HostCounters::default(),
            started_at: now(),
            started: std::time::Instant::now(),
            recall_ms: live::Latencies::default(),
            embed_ms: live::Latencies::default(),
        });
        let worker = tokio::spawn(schedule::embed_worker(Arc::downgrade(&host)));
        lock(&host.tasks).push(worker);
        host
    }

    /// The host's configuration.
    pub fn config(&self) -> &HostConfig {
        &self.config
    }

    /// When this host started, and how long ago.
    pub(crate) fn started(&self) -> (Timestamp, Duration) {
        (self.started_at, self.started.elapsed())
    }

    /// The live events of an open store; `None` when the store is not open. Opens
    /// nothing and counts as no activity.
    pub(crate) fn subscribe(
        &self,
        id: StoreId,
    ) -> Option<tokio::sync::broadcast::Receiver<Arc<live::UiEvent>>> {
        let open = lock(&self.entries).get(&id).and_then(|e| e.open())?;
        Some(open.events.subscribe())
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
        let (open, permit) = loop {
            let open = self.open_store(ctx.store_id).await?;
            let permit = open.requests.clone().try_acquire_owned();
            // Read after taking the permit: a close that has not begun yet then waits for
            // this request (`close_open`). A store that is closing takes no new request:
            // wait until it is closed, its final snapshot written, or open again.
            if *open.closing.borrow() {
                drop(permit);
                let mut closing = open.closing.subscribe();
                drop(open);
                let _ = closing.wait_for(|c| !*c).await;
                continue;
            }
            break (open, permit.map_err(|_| ApiError::unavailable(1))?);
        };
        *lock(&open.last_request) = Instant::now();
        if let Some(client) = &ctx.client {
            note_client(&mut lock(&open.clients), client, now());
        }
        Ok((open, permit))
    }

    /// The MCP client names an open store has seen, most recent first, with when each
    /// was last seen (for the web UI's system status). Empty for a store not open.
    pub fn clients(&self, id: StoreId) -> Vec<(String, Timestamp)> {
        let open = lock(&self.entries).get(&id).and_then(|e| e.open());
        let mut list = open.map(|o| lock(&o.clients).clone()).unwrap_or_default();
        list.reverse();
        list
    }

    async fn open_store(&self, id: StoreId) -> Result<Arc<OpenStore>, ApiError> {
        let mut state = self.start_open(id)?.state.clone();
        let loaded = state
            .wait_for(Option::is_some)
            .await
            .map_err(|_| ApiError::unavailable(2))?;
        loaded
            .as_ref()
            .cloned()
            .unwrap_or_else(|| Err(ApiError::unavailable(2)))
    }

    /// A store's lock while it holds: for a minute after the last try, and for good when
    /// the store does not fit this container (it opens only in a new one, tab "Server").
    fn cached_lock(&self, id: StoreId) -> Option<LockReason> {
        let (reason, at) = lock(&self.locks).get(&id).copied()?;
        (reason == LockReason::OutOfMemory || at.elapsed() < LOCK_RETRY).then_some(reason)
    }

    /// The store's entry, starting its load when there is none.
    fn start_open(&self, id: StoreId) -> Result<Arc<Entry>, ApiError> {
        if self.closing.load(Ordering::Relaxed) {
            return Err(ApiError::unavailable(2));
        }
        if let Some(reason) = self.cached_lock(id) {
            return Err(ApiError::store_locked(reason));
        }
        let mut entries = lock(&self.entries);
        if let Some(entry) = entries.get(&id) {
            return Ok(entry.clone());
        }
        let host = self.me.upgrade().ok_or_else(|| ApiError::unavailable(2))?;
        let (tx, state) = watch::channel(None);
        let entry = Arc::new(Entry { state });
        entries.insert(id, entry.clone());
        drop(entries);
        let mine = entry.clone();
        let task = tokio::spawn(async move {
            let result = host.load(id).await.map_err(|failure| {
                if let OpenFailure::Locked(reason) = failure {
                    lock(&host.locks).insert(id, (reason, Instant::now()));
                }
                let mut entries = lock(&host.entries);
                if entries.get(&id).is_some_and(|e| Arc::ptr_eq(e, &mine)) {
                    entries.remove(&id);
                }
                ApiError::from(failure)
            });
            tx.send_replace(Some(result));
        });
        self.own(task);
        Ok(entry)
    }

    /// Keeps a spawned task's handle, so that `shutdown` can stop it.
    fn own(&self, task: JoinHandle<()>) {
        let mut tasks = lock(&self.tasks);
        tasks.retain(|t| !t.is_finished());
        tasks.push(task);
    }

    /// Loading a store (tab "Snapshots and keys", "Loading a store").
    async fn load(&self, id: StoreId) -> Result<Arc<OpenStore>, OpenFailure> {
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
                let (mut store, snapshot_bytes) = match snapshot {
                    None => (Store::create(id, model.clone(), now), None),
                    Some(source) => {
                        let (store, len) = read_snapshot(id, source, &read_key, max_len)?;
                        (store, Some(len))
                    }
                };
                let saved = store.changes();
                store.on_open(now);
                if *store.model() != model {
                    store.adopt_model(model);
                }
                store.sweep(now);
                Ok::<_, LoadError>(Decoded {
                    saved,
                    current: store.changes(),
                    summary: store.summary(),
                    estimated: store.estimated_bytes(),
                    snapshot_bytes,
                    store,
                })
            })
            .await
            .map_err(|e| OpenFailure::Other(ApiError::from_join(&e)))?;
            match loaded {
                Ok(decoded) => {
                    let restored = if restore { name } else { None };
                    return Ok(self.install(
                        id,
                        decoded,
                        key,
                        wrapped,
                        generation,
                        next_sequence,
                        restored,
                    ));
                }
                Err(LoadError::FormatTooNew) => {
                    return Err(OpenFailure::Locked(LockReason::FormatTooNew));
                }
                Err(LoadError::TooLarge) => {
                    return Err(OpenFailure::Locked(LockReason::OutOfMemory));
                }
                Err(LoadError::Transit) => {
                    return Err(OpenFailure::Other(ApiError::unavailable(2)));
                }
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
                        return Err(OpenFailure::Locked(LockReason::SnapshotUnreadable));
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
        decoded: Decoded,
        key: DataKey,
        wrapped: WrappedKey,
        generation: u64,
        next_sequence: u64,
        restored: Option<SnapshotName>,
    ) -> Arc<OpenStore> {
        let Decoded {
            store,
            saved,
            current,
            summary,
            estimated,
            snapshot_bytes,
        } = decoded;
        let concurrency = u32::try_from(self.config.limits.store_concurrency).unwrap_or(u32::MAX);
        let open = Arc::new(OpenStore {
            id,
            store: Arc::new(RwLock::new(store)),
            gate: Arc::new(Semaphore::new(1)),
            requests: Arc::new(Semaphore::new(concurrency as usize)),
            concurrency,
            closing: watch::Sender::new(false),
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
            events: live::EventLog::new(),
            last_sweep: Mutex::new(now()),
            snapshot_failed: AtomicBool::new(false),
            snapshot_bytes: Mutex::new(snapshot_bytes),
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
            // Not in `open_list` until its load is announced, so add it by hand.
            metrics::gauge!("knoggin_embed_queue_depth")
                .increment(summary.pending_embeddings as f64);
            self.work.notify_one();
        }
        open
    }

    fn backend_error(&self, id: StoreId, e: BackendError) -> OpenFailure {
        OpenFailure::Other(match e {
            BackendError::Locked(reason) => return OpenFailure::Locked(reason),
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
            BackendError::Internal => ApiError::internal(),
        })
    }

    /// The engine's estimate summed over every open store.
    fn total_estimated(&self) -> u64 {
        self.open_list()
            .iter()
            .map(|o| o.estimated.load(Ordering::Relaxed))
            .fold(0, u64::saturating_add)
    }

    /// Sets `knoggin_embed_queue_depth`: the memories waiting in every open store's queue.
    fn report_queue_depth(&self) {
        let depth = self
            .open_list()
            .iter()
            .map(|o| o.pending.load(Ordering::Relaxed))
            .fold(0, u64::saturating_add);
        metrics::gauge!("knoggin_embed_queue_depth").set(depth as f64);
    }

    /// The open stores, by ID.
    fn open_list(&self) -> Vec<Arc<OpenStore>> {
        let mut list: Vec<Arc<OpenStore>> = lock(&self.entries)
            .values()
            .filter_map(|e| e.open())
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
        self.finish(open, out, false)?
    }

    /// Runs `f` under the write lock in a blocking task, after the write gate, and arms
    /// the snapshot timers for what it changed.
    pub(crate) async fn write_on<R: Send + 'static>(
        &self,
        open: &Arc<OpenStore>,
        kind: WriteKind,
        f: impl FnOnce(&mut Store) -> Result<R, ApiError> + Send + 'static,
    ) -> Result<R, ApiError> {
        let gate = open
            .gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ApiError::unavailable(1))?;
        // A closed store has written its final snapshot: a write now would be lost.
        if open.dropped.load(Ordering::SeqCst) {
            return Err(ApiError::unavailable(1));
        }
        // After the gate, so that writes queued behind one another each see the total
        // the one before them left.
        self.admit(open, kind)?;
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
        let result = self.finish(open, out, true)?;
        self.report_queue_depth();
        if result.is_ok() && kind != WriteKind::Bookkeeping {
            open.full.store(false, Ordering::Relaxed);
        }
        if kind == WriteKind::MakesRoom {
            self.recheck_budget();
        }
        result
    }

    /// The answer of a blocking store operation. After a panic in a write the store is
    /// dropped unsaved, because it may be half changed; a read changes nothing, so a panic
    /// there loses only that request.
    fn finish<R>(
        &self,
        open: &Arc<OpenStore>,
        out: Result<Result<Result<R, ApiError>, Poisoned>, tokio::task::JoinError>,
        wrote: bool,
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
                if join.is_panic() && wrote {
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
        let open = lock(&self.entries).get(&id).and_then(|e| e.open());
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
                .and_then(|e| e.open())
                .is_some_and(|o| Arc::ptr_eq(&o, open))
            {
                entries.remove(&open.id);
            }
        }
        if let Some(task) = lock(&open.task).take() {
            task.abort();
        }
        metrics::gauge!("knoggin_open_stores").decrement(1.0);
        self.recheck_budget();
        self.report_queue_depth();
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
        let Some(open) = lock(&self.entries).get(&id).and_then(|e| e.open()) else {
            return Ok(());
        };
        self.close_open(&open).await
    }

    async fn close_open(&self, open: &Arc<OpenStore>) -> Result<(), BackendError> {
        // New requests wait (`open_for`), the requests in flight and the write holding the
        // gate end first, and a write of the host's own tasks that comes after finds the
        // store dropped: nothing is accepted after the final snapshot.
        open.closing.send_replace(true);
        let _reopen = Reopen(&open.closing);
        let _requests = open
            .requests
            .acquire_many(open.concurrency)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        let _gate = open
            .gate
            .acquire()
            .await
            .map_err(|_| BackendError::Unavailable)?;
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
            return match entry.open() {
                Some(open) => StoreStatus {
                    state: StoreState::Ready,
                    full: open.is_full(),
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
        let locked = lock(&self.locks).get(&id).map(|l| l.0);
        let state = match locked {
            Some(reason) => {
                if self.cached_lock(id).is_none() {
                    // Starts the load in the background; its result is not waited for.
                    let _ = self.start_open(id);
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
            match entry.1.open() {
                Some(open) => report.stores.push(OpenStoreReport {
                    store_id: open.id,
                    memory_count: open.memory_count.load(Ordering::Relaxed),
                    estimated_bytes: open.estimated.load(Ordering::Relaxed),
                    unsaved: open.unsaved(),
                    last_sequence: open.last_sequence.load(Ordering::Relaxed),
                    pending_embeddings: open.pending.load(Ordering::Relaxed),
                    total_embeddable: open.embeddable.load(Ordering::Relaxed),
                    full: open.is_full(),
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

    /// Whether a store is open in memory.
    pub fn is_open(&self, id: StoreId) -> bool {
        lock(&self.entries)
            .get(&id)
            .is_some_and(|e| e.open().is_some())
    }

    pub(crate) fn notify_work(&self) {
        self.work.notify_one();
    }
}

/// Ends a close, however it ended: the requests that waited for it look again.
struct Reopen<'a>(&'a watch::Sender<bool>);

impl Drop for Reopen<'_> {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

/// A store lock was poisoned by a panic.
struct Poisoned;

/// Why a store could not be opened.
enum OpenFailure {
    /// Locked: remembered, and tried again at most once a minute.
    Locked(LockReason),
    /// Anything else; the next request tries again.
    Other(ApiError),
}

impl From<OpenFailure> for ApiError {
    fn from(failure: OpenFailure) -> Self {
        match failure {
            OpenFailure::Locked(reason) => ApiError::store_locked(reason),
            OpenFailure::Other(e) => e,
        }
    }
}

/// Counts the bytes read through it.
struct Counted<R> {
    inner: R,
    read: u64,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n as u64;
        Ok(n)
    }
}

/// Reads, checks, decrypts and decodes one snapshot, on a blocking thread. Returns the
/// store and the snapshot's size.
fn read_snapshot(
    id: StoreId,
    source: SnapshotSource,
    key: &DataKey,
    max_len: u64,
) -> Result<(Store, u64), LoadError> {
    let SnapshotSource { name, reader } = source;
    let mut reader = Counted {
        inner: reader,
        read: 0,
    };
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
    let store = snapshot::check_content_format(header.content_format)
        .and_then(|()| Store::decode(header.content_format, &content))
        .map_err(|e| match e {
            SnapshotError::FormatTooNew(_) => LoadError::FormatTooNew,
            SnapshotError::Corrupt(_) => LoadError::Corrupt,
        })?;
    Ok((store, reader.read))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::Permissions;
    use crate::testing::{TestHost, fake_config};

    async fn host() -> (tempfile::TempDir, TestHost) {
        let dir = tempfile::tempdir().unwrap();
        let t = TestHost::new(dir.path(), fake_config(), false, EmbedWaits::default()).await;
        (dir, t)
    }

    async fn save(t: &TestHost, summary: &str) {
        let req = serde_json::from_value(json!({ "summary": summary })).unwrap();
        let ctx = t.ctx(Permissions::READ_WRITE);
        t.host.store_memory(&ctx, req, None).await.unwrap();
    }

    async fn count(t: &TestHost) -> usize {
        let ctx = t.ctx(Permissions::READ_WRITE);
        let req = serde_json::from_value(json!({})).unwrap();
        t.host.list_memories(&ctx, req).await.unwrap().items.len()
    }

    async fn opened(t: &TestHost) -> Arc<OpenStore> {
        t.host.open_for(&t.ctx(Permissions::READ)).await.unwrap().0
    }

    #[tokio::test]
    async fn a_panic_in_a_write_drops_the_store_without_saving_it() {
        let (_dir, t) = host().await;
        save(&t, "saved").await;
        t.host.close(t.store).await.unwrap();
        save(&t, "lost with the panic").await;
        let saved = t.host.counters.snapshot_count.load(Ordering::SeqCst);
        let open = opened(&t).await;
        let e = t
            .host
            .write_on(
                &open,
                WriteKind::Client,
                |_s: &mut Store| -> Result<(), ApiError> { panic!("marker-in-a-store-operation") },
            )
            .await
            .unwrap_err();
        assert_eq!(
            (e.code, e.message.as_str()),
            (Code::Internal, "Internal error")
        );
        assert!(!t.host.is_open(t.store));
        assert!(t.host.save_all().await.is_empty());
        assert_eq!(
            t.host.counters.snapshot_count.load(Ordering::SeqCst),
            saved,
            "the half-changed store was not saved"
        );
        // The next request loads the last snapshot.
        assert_eq!(count(&t).await, 1);
    }

    /// How often the embedding worker went idle in 200 ms of real time: a spinning worker
    /// would keep paused time from ever moving.
    async fn idle_waits_in_a_while(host: &StoreHost) -> u64 {
        let before = host.idle_waits.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(200)).await;
        host.idle_waits.load(Ordering::Relaxed) - before
    }

    #[tokio::test]
    async fn the_embedding_worker_waits_without_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let TestHost { host, model, .. } =
            TestHost::new(dir.path(), fake_config(), true, EmbedWaits::default()).await;
        // The model state has changed once (to ready).
        let waits = idle_waits_in_a_while(&host).await;
        assert!(waits <= 2, "after a change: idle {waits} times");
        drop(model);
        let waits = idle_waits_in_a_while(&host).await;
        assert!(waits <= 2, "without a sender: idle {waits} times");
    }

    #[tokio::test]
    async fn a_panic_in_a_read_keeps_the_store_and_its_changes() {
        let (_dir, t) = host().await;
        save(&t, "not saved yet").await;
        let open = opened(&t).await;
        let e = t
            .host
            .read_on(&open, |_s: &Store| -> Result<(), ApiError> {
                panic!("marker-in-a-read")
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, Code::Internal);
        assert!(t.host.is_open(t.store), "a read changes nothing");
        assert!(t.host.save_all().await.is_empty());
        t.host.drop_store(t.store);
        assert_eq!(count(&t).await, 1, "the change was saved");
    }

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
