// SPDX-License-Identifier: AGPL-3.0-only
//! The `Embedder`: two OS threads, one per provider (tab "Embeddings", Threads and the
//! queue). A recall never waits behind a save, and a waiting duplicate check goes before
//! queue work. The async side talks to the threads through `tokio::sync` channels and
//! never blocks a runtime thread.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use knoggin_core::embed::{EmbedError, EmbeddingProvider, ModelIdentity};
use knoggin_core::ops::{ModelState, ModelStatus};
use tokio::sync::{OnceCell, mpsc, oneshot, watch};

/// The two loaded providers. The binary fills a `OnceCell` with them once they load.
pub struct Providers {
    /// `Role::Query`.
    pub query: Arc<dyn EmbeddingProvider>,
    /// `Role::Document`.
    pub document: Arc<dyn EmbeddingProvider>,
}

impl fmt::Debug for Providers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Providers")
            .field("identity", &self.document.identity().id)
            .finish_non_exhaustive()
    }
}

/// How long a request waits for a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedWaits {
    /// A recall's query.
    pub query: Duration,
    /// A save that asked for the duplicate check.
    pub duplicate_check: Duration,
}

impl Default for EmbedWaits {
    fn default() -> Self {
        Self {
            query: Duration::from_secs(10),
            duplicate_check: Duration::from_secs(10),
        }
    }
}

/// What a queue job got from the document thread.
#[derive(Debug)]
pub(crate) enum QueueOutcome {
    /// The vector.
    Vector(Vec<f32>),
    /// The model refused the text: count a failure.
    Failed,
    /// The model is not ready: leave the job for later.
    NotReady,
}

type Reply = oneshot::Sender<Result<Vec<f32>, EmbedError>>;

struct Job {
    text: String,
    reply: Reply,
}

struct DocJob {
    urgent: bool,
    job: Job,
}

/// The senders of the two threads. Dropping them (with the `Embedder`) ends both threads
/// once their current text is done.
struct Threads {
    query: mpsc::UnboundedSender<Job>,
    document: mpsc::UnboundedSender<DocJob>,
    _handles: Vec<JoinHandle<()>>,
}

/// Two providers on two OS threads, shared by every open store of the process.
pub struct Embedder {
    state: watch::Receiver<ModelState>,
    providers: Arc<OnceCell<Providers>>,
    waits: EmbedWaits,
    threads: OnceLock<Threads>,
}

impl fmt::Debug for Embedder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Embedder")
            .field("state", &self.state.borrow().state)
            .field("waits", &self.waits)
            .finish_non_exhaustive()
    }
}

impl Embedder {
    /// An embedder over the model state channel and the cell the binary fills with the
    /// providers. The threads start once the providers are there.
    pub fn new(
        state: watch::Receiver<ModelState>,
        providers: Arc<OnceCell<Providers>>,
        waits: EmbedWaits,
    ) -> Self {
        Self {
            state,
            providers,
            waits,
            threads: OnceLock::new(),
        }
    }

    /// The current model state.
    pub fn state(&self) -> ModelState {
        self.state.borrow().clone()
    }

    /// A receiver that wakes on every change of the model state.
    pub(crate) fn subscribe(&self) -> watch::Receiver<ModelState> {
        self.state.clone()
    }

    /// The identity of the loaded providers, when the model is ready.
    pub(crate) fn ready_identity(&self) -> Option<ModelIdentity> {
        if self.state.borrow().state != ModelStatus::Ready {
            return None;
        }
        self.providers.get().map(|p| p.document.identity().clone())
    }

    fn threads(&self) -> Option<&Threads> {
        self.ready_identity()?;
        let providers = self.providers.clone();
        Some(self.threads.get_or_init(|| start_threads(providers)))
    }

    /// The query vector, or `None` when the model is not ready, fails or takes longer
    /// than the query wait. The caller then runs the recall without a vector.
    pub(crate) async fn query(&self, text: String) -> Option<Vec<f32>> {
        let threads = self.threads()?;
        let (reply, rx) = oneshot::channel();
        threads.query.send(Job { text, reply }).ok()?;
        let start = Instant::now();
        let out = tokio::time::timeout(self.waits.query, rx).await;
        metrics::histogram!("knoggin_embed_seconds", "role" => "query")
            .record(start.elapsed().as_secs_f64());
        out.ok()?.ok()?.ok()
    }

    /// A duplicate check's vector, as urgent work, within the duplicate-check wait.
    pub(crate) async fn urgent(&self, text: String) -> Option<Vec<f32>> {
        let rx = self.document(text, true)?;
        tokio::time::timeout(self.waits.duplicate_check, rx)
            .await
            .ok()?
            .ok()?
            .ok()
    }

    /// A queue job's vector. No time limit: a queue job only waits behind urgent work.
    pub(crate) async fn queue(&self, text: String) -> QueueOutcome {
        let Some(rx) = self.document(text, false) else {
            return QueueOutcome::NotReady;
        };
        match rx.await {
            Ok(Ok(vector)) => QueueOutcome::Vector(vector),
            Ok(Err(_)) => QueueOutcome::Failed,
            // The thread is gone (a provider panicked): nothing can embed any more.
            Err(_) => QueueOutcome::NotReady,
        }
    }

    fn document(
        &self,
        text: String,
        urgent: bool,
    ) -> Option<oneshot::Receiver<Result<Vec<f32>, EmbedError>>> {
        let threads = self.threads()?;
        let (reply, rx) = oneshot::channel();
        threads
            .document
            .send(DocJob {
                urgent,
                job: Job { text, reply },
            })
            .ok()?;
        Some(rx)
    }
}

fn start_threads(providers: Arc<OnceCell<Providers>>) -> Threads {
    let (query_tx, mut query_rx) = mpsc::unbounded_channel::<Job>();
    let (doc_tx, mut doc_rx) = mpsc::unbounded_channel::<DocJob>();
    let mut handles = Vec::new();
    let p = providers.clone();
    let spawned = std::thread::Builder::new()
        .name("knoggin-embed-query".into())
        .spawn(move || {
            while let Some(job) = query_rx.blocking_recv() {
                if let Some(providers) = p.get() {
                    run(&*providers.query, job);
                }
            }
        });
    handles.extend(spawned.ok());
    let spawned = std::thread::Builder::new()
        .name("knoggin-embed-document".into())
        .spawn(move || {
            let (mut urgent, mut queue) = (VecDeque::new(), VecDeque::new());
            loop {
                if urgent.is_empty() && queue.is_empty() {
                    match doc_rx.blocking_recv() {
                        Some(j) => sort(j, &mut urgent, &mut queue),
                        None => return,
                    }
                }
                while let Ok(j) = doc_rx.try_recv() {
                    sort(j, &mut urgent, &mut queue);
                }
                let Some(job) = urgent.pop_front().or_else(|| queue.pop_front()) else {
                    continue;
                };
                if let Some(providers) = providers.get() {
                    let start = Instant::now();
                    run(&*providers.document, job);
                    metrics::histogram!("knoggin_embed_seconds", "role" => "document")
                        .record(start.elapsed().as_secs_f64());
                }
            }
        });
    handles.extend(spawned.ok());
    Threads {
        query: query_tx,
        document: doc_tx,
        _handles: handles,
    }
}

fn sort(job: DocJob, urgent: &mut VecDeque<Job>, queue: &mut VecDeque<Job>) {
    if job.urgent {
        urgent.push_back(job.job);
    } else {
        queue.push_back(job.job);
    }
}

fn run(provider: &dyn EmbeddingProvider, job: Job) {
    // A caller that stopped waiting (a timeout) needs no vector.
    if job.reply.is_closed() {
        return;
    }
    let _ = job.reply.send(provider.embed(&job.text));
}
