// SPDX-License-Identifier: AGPL-3.0-only
//! The `knoggin serve` role: one user container. It listens on the
//! internal link with a certificate it made itself, proves itself in `hello`, takes one
//! user in `claim`, serves the store layer for that user behind the router token, saves
//! through [`RouterBackend`], and parks (saves, reports `parked`, returns) after its idle
//! time, on the router's `park` call or on a stop signal.
//!
//! The role never exits the process: [`serve`] returns its exit code and why it ended,
//! and `main` (or the fleet tests' fake launcher) acts on that.

mod backend;
mod routes;

use std::collections::VecDeque;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use knoggin_core::UserId;
use knoggin_core::embed::{EmbeddingProvider, ModelIdentity};
use knoggin_core::ops::{ModelState, ModelStatus};
use knoggin_core::testing::FakeProvider;
use knoggin_http::host::{BackendError, Providers, StoreHost};
use tokio::net::TcpListener;
use tokio::sync::{Notify, OnceCell, Semaphore, watch};
use tokio::time::Instant;

pub use backend::RouterBackend;

use self::backend::Link;
use crate::launcher::{BuildInfo, ContainerEnv};
use crate::link::{ServerIdentity, Token, router_client, serve_tls, server_config};
use crate::lock;

/// Request bodies a container holds in flight at most.
pub const BODY_BYTES_IN_FLIGHT: u32 = 64 * 1024 * 1024;
/// While an open store has embedding work, an idle park waits at most this long after the
/// last request.
const EMBED_ALLOWANCE: Duration = Duration::from_secs(30 * 60);
/// How often an idle container whose park waits for the embedding queue looks again.
const IDLE_RECHECK: Duration = Duration::from_secs(60);
/// The longest pause between two tries of the final snapshots after a stop signal.
const MAX_SAVE_PAUSE: Duration = Duration::from_secs(60);

/// The embedding model as the role gets it: the binary's model loop, or the fake one of
/// test mode.
pub struct ServeModel {
    /// The running model's identity.
    pub identity: ModelIdentity,
    /// The Model state; `hello` answers once it is `ready`.
    pub state: watch::Receiver<ModelState>,
    /// Filled with the two providers once they have loaded.
    pub providers: Arc<OnceCell<Providers>>,
    /// Held for the role's life when the role owns the sender (the fake model): once
    /// every sender is gone, a wait on the channel returns at once and spins.
    _keep: Option<watch::Sender<ModelState>>,
}

impl std::fmt::Debug for ServeModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeModel")
            .field("identity", &self.identity.id)
            .finish_non_exhaustive()
    }
}

impl ServeModel {
    /// The binary's model; the caller keeps the state's sender alive.
    pub fn new(
        identity: ModelIdentity,
        state: watch::Receiver<ModelState>,
        providers: Arc<OnceCell<Providers>>,
    ) -> Self {
        Self {
            identity,
            state,
            providers,
            _keep: None,
        }
    }

    /// Test mode: the fake provider on both threads, `ready` at once.
    pub fn fake() -> Self {
        let fake = FakeProvider::new();
        let identity = fake.identity().clone();
        let (tx, rx) = watch::channel(ModelState {
            state: ModelStatus::Ready,
            identity: identity.id.clone(),
            name: "Fake model".into(),
            bytes_done: None,
            bytes_total: None,
            reason: None,
        });
        let cell = OnceCell::new();
        let _ = cell.set(Providers {
            query: Arc::new(FakeProvider::new()),
            document: Arc::new(fake),
        });
        Self {
            identity,
            state: rx,
            providers: Arc::new(cell),
            _keep: Some(tx),
        }
    }
}

/// The waits of the park sequence. Tests shorten them; everything else uses the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeTimings {
    /// How long a park waits for the requests in flight before it cuts them off (30 s).
    pub drain: Duration,
    /// How often a failed park is tried again while a router's park request stands (60 s).
    pub park_retry: Duration,
}

impl Default for ServeTimings {
    fn default() -> Self {
        Self {
            drain: Duration::from_secs(30),
            park_retry: Duration::from_secs(60),
        }
    }
}

/// What the container role is started with. No role function reads the environment.
#[derive(Debug)]
pub struct ServeConfig {
    /// The five values the launcher set.
    pub env: ContainerEnv,
    /// This build, reported in `hello` and `status`.
    pub build: BuildInfo,
    /// Test mode, reported in `hello`.
    pub test_mode: bool,
    /// The local address of the client to the router; only the fleet tests set it.
    pub local_address: Option<IpAddr>,
    /// The embedding model.
    pub model: ServeModel,
    /// The park sequence's waits.
    pub timings: ServeTimings,
}

/// Why a container role ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// It saved every store and reported `parked` (or tried to).
    Parked,
    /// A pool container that was never claimed got a stop signal or a park call.
    Stopped,
    /// The broker answered `lease_lost`: it stopped at once and wrote nothing more.
    LeaseLost,
    /// It could not start: no certificate, no TLS config, no client.
    Failed,
}

/// What [`serve`] returns: the exit code for `main` and why the role ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeEnd {
    /// The process's exit code: 0 after a park or a clean stop, 1 otherwise.
    pub exit_code: i32,
    /// Why.
    pub reason: EndReason,
}

impl ServeEnd {
    fn of(reason: EndReason) -> Self {
        let exit_code = match reason {
            EndReason::Parked | EndReason::Stopped => 0,
            EndReason::LeaseLost | EndReason::Failed => 1,
        };
        Self { exit_code, reason }
    }
}

/// Where the container stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Ready,
    Claimed,
    Parking,
}

/// The user a container was claimed for, and the host that serves them.
struct Claim {
    user: UserId,
    generation: u64,
    host: Arc<StoreHost>,
    store: Router,
    idle: Duration,
    /// `limits.body_bytes`: what a body of unknown length reserves of the 64 MiB.
    body_bytes: u64,
}

/// Requests that count as activity: how many are in flight and when one last started or
/// finished.
#[derive(Debug)]
struct Activity {
    in_flight: u64,
    last: Instant,
}

/// The role's state, shared by its routes and its main loop.
pub(crate) struct Shared {
    env: ContainerEnv,
    router_token: Token,
    build: BuildInfo,
    test_mode: bool,
    model: ServeModel,
    cert_der: Vec<u8>,
    timings: ServeTimings,
    link: Arc<Link>,
    phase: Mutex<Phase>,
    claim: OnceLock<Arc<Claim>>,
    activity: Mutex<Activity>,
    bodies: Arc<Semaphore>,
    /// A router's park request stands.
    park_stands: AtomicBool,
    /// Wakes the main loop: a claim, a park call, a request that finished.
    wake: Notify,
    /// True while a park cuts off the requests that outlived its wait.
    cut: watch::Sender<bool>,
    /// The last `hello` nonces, refused if sent again.
    nonces: Mutex<VecDeque<String>>,
}

/// Counts one request as activity until dropped.
pub(crate) struct ActivityGuard(Arc<Shared>);

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        {
            let mut a = lock(&self.0.activity);
            a.in_flight = a.in_flight.saturating_sub(1);
            a.last = Instant::now();
        }
        self.0.wake.notify_one();
    }
}

impl Shared {
    fn phase(&self) -> Phase {
        *lock(&self.phase)
    }

    fn set_phase(&self, phase: Phase) {
        *lock(&self.phase) = phase;
    }

    fn begin(self: &Arc<Self>) -> ActivityGuard {
        let mut a = lock(&self.activity);
        a.in_flight += 1;
        a.last = Instant::now();
        drop(a);
        ActivityGuard(self.clone())
    }

    /// Requests in flight, and when one last started or finished.
    fn activity(&self) -> (u64, Instant) {
        let a = lock(&self.activity);
        (a.in_flight, a.last)
    }

    /// Whole seconds since the last activity; 0 while a request is in flight.
    fn idle_seconds(&self) -> u64 {
        match self.activity() {
            (0, last) => last.elapsed().as_secs(),
            _ => 0,
        }
    }

    fn lease_lost(&self) -> bool {
        *self.link.lost.borrow()
    }

    /// When an idle park is due, given the last failed park.
    fn idle_at(&self, claim: &Claim, failed_at: Option<Instant>) -> Instant {
        let (in_flight, last) = self.activity();
        let since = failed_at.map_or(last, |f| f.max(last));
        if in_flight > 0 {
            Instant::now() + claim.idle
        } else {
            since + claim.idle
        }
    }

    /// Whether an idle park is due: nothing in flight for `idleSeconds`, and no store
    /// with embedding work unless 30 minutes have passed since the last request.
    fn idle_due(&self, claim: &Claim, failed_at: Option<Instant>) -> bool {
        let now = Instant::now();
        if self.phase() != Phase::Claimed || now < self.idle_at(claim, failed_at) {
            return false;
        }
        let (_, last) = self.activity();
        let queued = claim
            .host
            .report()
            .stores
            .iter()
            .any(|s| s.pending_embeddings > 0);
        !queued || now >= last + EMBED_ALLOWANCE
    }

    /// Whether a router's park request is due: at once, or a minute after a failed park.
    fn requested_due(&self, failed_at: Option<Instant>) -> bool {
        self.park_stands.load(Ordering::SeqCst)
            && failed_at.is_none_or(|f| Instant::now() >= f + self.timings.park_retry)
    }

    /// When the main loop looks again by itself.
    fn next_check(&self, failed_at: Option<Instant>) -> Instant {
        let now = Instant::now();
        let mut at = now + Duration::from_secs(24 * 3600);
        if self.park_stands.load(Ordering::SeqCst) {
            at = at.min(failed_at.map_or(now, |f| f + self.timings.park_retry));
        }
        if let Some(claim) = self.claim.get() {
            let idle = self.idle_at(claim, failed_at);
            at = at.min(if idle > now { idle } else { now + IDLE_RECHECK });
        }
        at
    }
}

/// The runtime `knoggin serve` runs on: multi-threaded, with at most 128 blocking
/// threads, because a user container may hold only 256 processes and threads.
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .max_blocking_threads(128)
        .build()
}

/// Runs the container role on `listen` (`0.0.0.0:8443` in the binary) until it parks,
/// loses its lease, or, unclaimed, gets `stop`. `stop` is the stop signal: a claimed
/// container then saves until it succeeds or the process is killed.
pub async fn serve(
    listen: SocketAddr,
    config: ServeConfig,
    stop: impl Future<Output = ()> + Send,
) -> ServeEnd {
    match TcpListener::bind(listen).await {
        Ok(listener) => serve_on(listener, config, stop).await,
        Err(e) => {
            tracing::error!(kind = ?e.kind(), "the container cannot listen");
            ServeEnd::of(EndReason::Failed)
        }
    }
}

/// [`serve`] on a listener the caller bound (the fleet tests bind port 0 and read it).
pub async fn serve_on(
    listener: TcpListener,
    config: ServeConfig,
    stop: impl Future<Output = ()> + Send,
) -> ServeEnd {
    let Some((shared, tls)) = build(config) else {
        return ServeEnd::of(EndReason::Failed);
    };
    let (stop_server, server_stopped) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(serve_tls(
        listener,
        tls,
        routes::app(shared.clone()),
        |_| Some(()),
        async {
            let _ = server_stopped.await;
        },
    ));
    let end = drive(&shared, stop).await;
    if let Some(claim) = shared.claim.get() {
        claim.host.shutdown();
    }
    // After a lost lease the listener stops at once: nothing it left behind may still
    // answer or upload. Otherwise the requests in flight get a few seconds.
    let graceful = end.reason != EndReason::LeaseLost && {
        let _ = stop_server.send(());
        tokio::time::timeout(Duration::from_secs(5), &mut server)
            .await
            .is_ok()
    };
    if !graceful {
        server.abort();
        let _ = server.await;
    }
    tracing::info!(
        container_id = %shared.env.container_id,
        code = end.exit_code,
        "the container ends"
    );
    end
}

fn build(config: ServeConfig) -> Option<(Arc<Shared>, Arc<rustls::ServerConfig>)> {
    let ServeConfig {
        env,
        build,
        test_mode,
        local_address,
        model,
        timings,
    } = config;
    let identity = ServerIdentity::for_container(env.container_id).ok()?;
    let tls = server_config(&identity).ok()?;
    let client = router_client(env.router_cert_sha256, local_address).ok()?;
    let bucket = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(crate::link::CONNECT_TIMEOUT)
        .build()
        .ok()?;
    let link = Arc::new(Link {
        client,
        bucket,
        router_url: env.router_url.trim_end_matches('/').to_owned(),
        launch: env.launch_secret.clone(),
        container_id: env.container_id,
        lost: watch::Sender::new(false),
    });
    let shared = Arc::new(Shared {
        router_token: env.launch_secret.router_token(),
        env,
        build,
        test_mode,
        model,
        cert_der: identity.cert_der().to_vec(),
        timings,
        link,
        phase: Mutex::new(Phase::Ready),
        claim: OnceLock::new(),
        activity: Mutex::new(Activity {
            in_flight: 0,
            last: Instant::now(),
        }),
        bodies: Arc::new(Semaphore::new(BODY_BYTES_IN_FLIGHT as usize)),
        park_stands: AtomicBool::new(false),
        wake: Notify::new(),
        cut: watch::Sender::new(false),
        nonces: Mutex::new(VecDeque::new()),
    });
    Some((shared, tls))
}

/// The main loop: waits for a claim, then for a park trigger, and runs the park sequence:
/// (1) end the event streams and wait for the requests in flight, cutting off the rest;
/// (2) save every store; (3) report `parked`; (4) if a save failed, serve again.
async fn drive(s: &Arc<Shared>, stop: impl Future<Output = ()> + Send) -> ServeEnd {
    let mut stop = pin!(stop);
    let mut lost = s.link.lost.subscribe();
    let mut stopping = false;
    let mut failed_at: Option<Instant> = None;
    loop {
        let wake_at = s.next_check(failed_at);
        tokio::select! {
            () = &mut stop, if !stopping => stopping = true,
            _ = lost.wait_for(|l| *l) => return ServeEnd::of(EndReason::LeaseLost),
            () = s.wake.notified() => {}
            () = tokio::time::sleep_until(wake_at) => {}
        }
        let Some(claim) = s.claim.get().cloned() else {
            // A pool container holds nothing: it leaves at once.
            if stopping || s.park_stands.load(Ordering::SeqCst) {
                return ServeEnd::of(EndReason::Stopped);
            }
            continue;
        };
        if !(stopping || s.requested_due(failed_at) || s.idle_due(&claim, failed_at)) {
            continue;
        }
        let parked = tokio::select! {
            ok = park(s, &claim, stopping) => ok,
            _ = lost.wait_for(|l| *l) => return ServeEnd::of(EndReason::LeaseLost),
        };
        if parked {
            if !s.link.parked(claim.user).await {
                tracing::warn!(container_id = %s.env.container_id, "parked could not be reported");
            }
            if s.lease_lost() {
                return ServeEnd::of(EndReason::LeaseLost);
            }
            return ServeEnd::of(EndReason::Parked);
        }
        // Step 4: serve again, and try again at the next idle moment or every minute
        // while a router's park request stands.
        tracing::warn!(container_id = %s.env.container_id, "a park failed; serving again");
        failed_at = Some(Instant::now());
        s.cut.send_replace(false);
        s.set_phase(Phase::Claimed);
    }
}

/// Steps 1 and 2 of the park sequence; true when every store was saved. After a stop
/// signal the snapshots are tried again until they succeed or the process is killed.
async fn park(s: &Arc<Shared>, claim: &Claim, stopping: bool) -> bool {
    tracing::info!(container_id = %s.env.container_id, user_id = %claim.user, "parking");
    s.set_phase(Phase::Parking);
    claim.host.end_event_streams();
    let deadline = Instant::now() + s.timings.drain;
    while s.activity().0 > 0 && Instant::now() < deadline {
        tokio::select! {
            () = s.wake.notified() => {}
            () = tokio::time::sleep_until(deadline) => {}
        }
    }
    if s.activity().0 > 0 {
        s.cut.send_replace(true);
    }
    let mut pause = Duration::from_secs(1);
    loop {
        // A store the broker reports deleted was dropped; that is not a failed snapshot.
        let failed = claim
            .host
            .save_all()
            .await
            .into_iter()
            .filter(|(_, e)| *e != BackendError::Deleted)
            .count();
        if failed == 0 {
            return true;
        }
        if !stopping || s.lease_lost() {
            return false;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(MAX_SAVE_PAUSE);
    }
}
