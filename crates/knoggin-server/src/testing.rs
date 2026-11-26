// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers for the router: the fake [`Launcher`] of the fleet tests, which runs a
//! container role inside the test process on a loopback port and reports `endReason` and
//! `exitCode` as the launcher service does, and a stub container that speaks the link's
//! routes (`hello`, `claim`, `park`, `status`, forwarded store routes) with behaviours a
//! test can switch, for the router's tests that need no real store layer.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use futures_util::future::BoxFuture;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use knoggin_core::Timestamp;
use knoggin_core::UserId;
use knoggin_core::ops::{ModelState, ModelStatus};
use knoggin_http::ctx::headers;
use knoggin_state::{Clock, Item, StateDb, StateError, Write};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::container::{ServeConfig, ServeModel, ServeTimings, serve_on};
use crate::launcher::{
    ContainerEnv, ContainerInfo, ContainerSpec, EndReason, Ended, LaunchError, LaunchHandle,
    Launcher, RunState,
};
use crate::link::api::{self, ClaimRequest, ContainerPhase, Counters, HelloAnswer, StatusAnswer};
use crate::link::{
    LaunchSecret, Nonce, ServerIdentity, serve_tls, server_config, verify_router_call,
};

/// What a container role returns: its exit code.
pub type Exit = i64;

/// Runs one container: its environment, a listener bound to its address, and a signal
/// that the launcher asks it to stop. Returns the exit code.
pub type Runner = Arc<
    dyn Fn(ContainerEnv, TcpListener, oneshot::Receiver<()>) -> BoxFuture<'static, Exit>
        + Send
        + Sync,
>;

struct Fake {
    id: Uuid,
    address: SocketAddrV4,
    ended: Option<Ended>,
    stop: Option<oneshot::Sender<()>>,
    /// Ends the container's own runtime at once, with every task it runs.
    kill: Option<oneshot::Sender<()>>,
    forgotten: bool,
    draining: bool,
    started: Timestamp,
    /// Set when `stop` or `kill` decided the reason before the role returned.
    reason: Option<EndReason>,
}

/// A launcher that runs each container's role as a task of the test process.
pub struct FakeLauncher {
    runner: Mutex<Runner>,
    clock: Clock,
    containers: Arc<Mutex<HashMap<String, Fake>>>,
    next_ip: AtomicU8,
    /// Every call answers `Unavailable`.
    pub unreachable: AtomicBool,
    /// `launch` is refused.
    pub refuse: AtomicBool,
    /// `stop` fails.
    pub fail_stop: AtomicBool,
    /// The address each container gets; set to reuse one.
    pub fixed_address: Mutex<Option<SocketAddrV4>>,
    /// How long `stop` waits for a role to end before it kills it (10 seconds).
    pub stop_grace: Mutex<Duration>,
}

impl std::fmt::Debug for FakeLauncher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeLauncher").finish_non_exhaustive()
    }
}

impl FakeLauncher {
    /// A launcher whose containers run `runner`.
    pub fn new(runner: Runner, clock: Clock) -> Arc<Self> {
        Arc::new(Self {
            runner: Mutex::new(runner),
            clock,
            containers: Arc::new(Mutex::new(HashMap::new())),
            next_ip: AtomicU8::new(2),
            unreachable: AtomicBool::new(false),
            refuse: AtomicBool::new(false),
            fail_stop: AtomicBool::new(false),
            fixed_address: Mutex::new(None),
            stop_grace: Mutex::new(Duration::from_secs(10)),
        })
    }

    /// Runs later containers with another role (a second build).
    pub fn set_runner(&self, runner: Runner) {
        *self.runner.lock().unwrap() = runner;
    }

    /// The next address: its own `127.0.0.x` on Linux, `127.0.0.1` elsewhere.
    fn next_address(&self) -> Ipv4Addr {
        if cfg!(target_os = "linux") {
            let n = self.next_ip.fetch_add(1, Ordering::Relaxed).max(2);
            Ipv4Addr::new(127, 0, 0, n)
        } else {
            Ipv4Addr::LOCALHOST
        }
    }

    /// The handles of every container launched so far.
    pub fn handles(&self) -> Vec<LaunchHandle> {
        self.containers
            .lock()
            .unwrap()
            .keys()
            .map(|k| LaunchHandle(k.clone()))
            .collect()
    }

    /// The handle of the container with this Knoggin ID.
    pub fn handle_of(&self, id: Uuid) -> Option<LaunchHandle> {
        self.containers
            .lock()
            .unwrap()
            .iter()
            .find(|(_, c)| c.id == id)
            .map(|(h, _)| LaunchHandle(h.clone()))
    }

    /// How many containers run now.
    pub fn running(&self) -> usize {
        self.containers
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.ended.is_none())
            .count()
    }

    /// The launcher forgets a container: neither `describe` nor `list` shows it again.
    pub fn forget(&self, handle: &LaunchHandle) {
        if let Some(c) = self.containers.lock().unwrap().get_mut(&handle.0) {
            c.forgotten = true;
        }
    }

    /// Marks a container's host as draining.
    pub fn drain(&self, handle: &LaunchHandle) {
        if let Some(c) = self.containers.lock().unwrap().get_mut(&handle.0) {
            c.draining = true;
        }
    }

    /// Ends a container at once: killed for memory, or crashed with exit code 1.
    pub fn kill(&self, handle: &LaunchHandle, memory: bool) {
        let ended = if memory {
            Ended {
                reason: EndReason::Memory,
                exit_code: Some(137),
            }
        } else {
            Ended {
                reason: EndReason::Exited,
                exit_code: Some(1),
            }
        };
        self.end_now(handle, ended);
    }

    /// Ends a container's runtime at once and records `ended`.
    fn end_now(&self, handle: &LaunchHandle, ended: Ended) {
        let mut map = self.containers.lock().unwrap();
        if let Some(c) = map.get_mut(&handle.0) {
            if c.ended.is_none() {
                c.ended = Some(ended);
            }
            if let Some(k) = c.kill.take() {
                let _ = k.send(());
            }
        }
    }

    fn check(&self) -> Result<(), LaunchError> {
        if self.unreachable.load(Ordering::Relaxed) {
            Err(LaunchError::Unavailable)
        } else {
            Ok(())
        }
    }

    fn info(handle: &str, c: &Fake) -> ContainerInfo {
        ContainerInfo {
            handle: LaunchHandle(handle.to_owned()),
            container_id: Some(c.id),
            state: if c.ended.is_some() {
                RunState::Ended
            } else {
                RunState::Running
            },
            address: Some(c.address),
            host: Some(if c.draining { "draining-host" } else { "host" }.into()),
            draining: c.draining,
            started_at: Some(c.started),
            ended: c.ended,
        }
    }
}

#[async_trait]
impl Launcher for FakeLauncher {
    async fn launch(&self, spec: &ContainerSpec) -> Result<LaunchHandle, LaunchError> {
        self.check()?;
        if self.refuse.load(Ordering::Relaxed) {
            return Err(LaunchError::Refused("no capacity"));
        }
        let fixed = *self.fixed_address.lock().unwrap();
        let listener = match fixed {
            Some(a) => std::net::TcpListener::bind(SocketAddr::V4(a)),
            None => std::net::TcpListener::bind((self.next_address(), 0)),
        }
        .and_then(|l| l.set_nonblocking(true).map(|()| l))
        .map_err(|_| LaunchError::Refused("no address"))?;
        let Ok(SocketAddr::V4(address)) = listener.local_addr() else {
            return Err(LaunchError::Refused("no address"));
        };
        let handle = format!("fake-{}", Uuid::now_v7());
        let (stop_tx, stop_rx) = oneshot::channel();
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let runner = self.runner.lock().unwrap().clone();
        let env = spec.env.clone();
        let containers = self.containers.clone();
        let key = handle.clone();
        // Hold the map while the container starts and is recorded, so that a role that
        // ends at once still finds its entry.
        let mut map = self.containers.lock().unwrap();
        // Each container runs on a runtime of its own, like a process: a kill shuts it
        // down with every task it spawned, its listener included.
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            else {
                return;
            };
            let code = rt.block_on(async move {
                let Ok(listener) = TcpListener::from_std(listener) else {
                    return Some(1);
                };
                let role = runner(env, listener, stop_rx);
                tokio::select! {
                    code = role => Some(code),
                    _ = kill_rx => None,
                }
            });
            rt.shutdown_background();
            if let Some(code) = code
                && let Some(c) = containers.lock().unwrap().get_mut(&key)
                && c.ended.is_none()
            {
                c.ended = Some(Ended {
                    reason: c.reason.unwrap_or(EndReason::Exited),
                    exit_code: Some(code),
                });
            }
        });
        map.insert(
            handle.clone(),
            Fake {
                id: spec.container_id,
                address,
                ended: None,
                stop: Some(stop_tx),
                kill: Some(kill_tx),
                forgotten: false,
                draining: false,
                started: Timestamp::from_millis((self.clock)()),
                reason: None,
            },
        );
        Ok(LaunchHandle(handle))
    }

    async fn describe(&self, handles: &[LaunchHandle]) -> Result<Vec<ContainerInfo>, LaunchError> {
        self.check()?;
        let map = self.containers.lock().unwrap();
        Ok(handles
            .iter()
            .filter_map(|h| {
                map.get(&h.0)
                    .filter(|c| !c.forgotten)
                    .map(|c| Self::info(&h.0, c))
            })
            .collect())
    }

    async fn list(&self) -> Result<Vec<ContainerInfo>, LaunchError> {
        self.check()?;
        let map = self.containers.lock().unwrap();
        Ok(map
            .iter()
            .filter(|(_, c)| !c.forgotten && c.ended.is_none())
            .map(|(h, c)| Self::info(h, c))
            .collect())
    }

    async fn stop(&self, handle: &LaunchHandle, _reason: &str) -> Result<(), LaunchError> {
        self.check()?;
        if self.fail_stop.load(Ordering::Relaxed) {
            return Err(LaunchError::Unavailable);
        }
        {
            let mut map = self.containers.lock().unwrap();
            let Some(c) = map.get_mut(&handle.0) else {
                return Err(LaunchError::NotFound);
            };
            if c.ended.is_some() {
                return Ok(());
            }
            c.reason = Some(EndReason::Stopped);
            if let Some(s) = c.stop.take() {
                let _ = s.send(());
            }
        }
        // A role gets the stop timeout to end by itself (a claimed one saves first).
        let grace = *self.stop_grace.lock().unwrap();
        let deadline = tokio::time::Instant::now() + grace;
        while tokio::time::Instant::now() < deadline {
            let ended = self
                .containers
                .lock()
                .unwrap()
                .get(&handle.0)
                .is_some_and(|c| c.ended.is_some());
            if ended {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.end_now(
            handle,
            Ended {
                reason: EndReason::Stopped,
                exit_code: Some(137),
            },
        );
        Ok(())
    }
}

/// A [`Runner`] of the real container role (`container::serve_on`) of `build`, with the
/// fake model. On Linux its client to the router is bound to its own `127.0.0.x`.
pub fn serve_runner(build: &str, test_mode: bool, timings: ServeTimings) -> Runner {
    let build = build.to_owned();
    Arc::new(
        move |env: ContainerEnv, listener: TcpListener, stop: oneshot::Receiver<()>| {
            let local_address = listener
                .local_addr()
                .ok()
                .map(|a| a.ip())
                .filter(|_| cfg!(target_os = "linux"));
            let config = ServeConfig {
                env,
                build: crate::launcher::BuildInfo::new(build.clone()),
                test_mode,
                local_address,
                model: ServeModel::fake(),
                timings,
            };
            Box::pin(async move {
                let end = serve_on(listener, config, async {
                    let _ = stop.await;
                })
                .await;
                i64::from(end.exit_code)
            })
        },
    )
}

/// What a stub container does with forwarded requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Answers JSON describing what it received.
    Echo,
    /// Answers with `Set-Cookie`, `Location` and `WWW-Authenticate` headers.
    Hostile,
    /// Answers an HTML page.
    Html,
    /// Answers a redirect.
    Redirect,
    /// Answers 401.
    Unauthorized,
    /// Answers 503 with `Knoggin-Internal-Status: parking`.
    Parking,
    /// Never answers.
    Hang,
    /// Its `status` never answers either.
    Dead,
}

/// Shared knobs of a stub container.
#[derive(Debug)]
pub struct StubControl {
    /// What forwarded requests get.
    pub behaviour: Mutex<Behaviour>,
    /// Refuse claims.
    pub refuse_claim: AtomicBool,
    /// Requests seen (headers only).
    pub seen: Mutex<Vec<HeaderMap>>,
    /// Exit (code 0) after a `park` call.
    pub exit_on_park: AtomicBool,
    /// Never answer a `park` call.
    pub hang_park: AtomicBool,
}

impl Default for StubControl {
    fn default() -> Self {
        Self {
            behaviour: Mutex::new(Behaviour::Echo),
            refuse_claim: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
            exit_on_park: AtomicBool::new(true),
            hang_park: AtomicBool::new(false),
        }
    }
}

#[derive(Clone)]
struct Stub {
    id: Uuid,
    launch: LaunchSecret,
    cert_der: Arc<Vec<u8>>,
    build: String,
    test_mode: bool,
    control: Arc<StubControl>,
    claim: Arc<Mutex<Option<(UserId, u64)>>>,
    exit: watch::Sender<Option<Exit>>,
}

fn model() -> ModelState {
    ModelState {
        state: ModelStatus::Ready,
        identity: "fake-64".into(),
        name: "Fake".into(),
        bytes_done: None,
        bytes_total: None,
        reason: None,
    }
}

fn json(status: StatusCode, value: &impl serde::Serialize) -> Response {
    (status, crate::json(value)).into_response()
}

#[derive(serde::Deserialize)]
struct HelloQuery {
    nonce: String,
}

async fn hello(State(s): State<Stub>, Query(q): Query<HelloQuery>) -> Response {
    let Some(nonce) = Nonce::from_hex(&q.nonce) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    json(
        StatusCode::OK,
        &HelloAnswer {
            container_id: s.id,
            build: s.build.clone(),
            protocols: api::PROTOCOLS.to_vec(),
            model: model(),
            test_mode: s.test_mode,
            proof: s
                .launch
                .hello_proof(&nonce, &s.cert_der)
                .expose()
                .to_owned(),
        },
    )
}

async fn claim(State(s): State<Stub>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if verify_router_call(&s.launch.router_token(), &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if s.control.refuse_claim.load(Ordering::Relaxed) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let Ok(req) = serde_json::from_slice::<ClaimRequest>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut claim = s.claim.lock().unwrap();
    if claim.is_some() {
        return json(
            StatusCode::CONFLICT,
            &serde_json::json!({"error": {"code": "conflict"}}),
        );
    }
    *claim = Some((req.user_id, req.generation));
    json(StatusCode::OK, &serde_json::json!({}))
}

async fn park(State(s): State<Stub>, headers: HeaderMap) -> Response {
    if verify_router_call(&s.launch.router_token(), &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if s.control.hang_park.load(Ordering::Relaxed) {
        std::future::pending::<()>().await;
    }
    if s.control.exit_on_park.load(Ordering::Relaxed) {
        // Exit once this answer is on its way.
        let exit = s.exit.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = exit.send(Some(0));
        });
    }
    StatusCode::ACCEPTED.into_response()
}

async fn status(State(s): State<Stub>, headers: HeaderMap) -> Response {
    if *s.control.behaviour.lock().unwrap() == Behaviour::Dead {
        std::future::pending::<()>().await;
    }
    if verify_router_call(&s.launch.router_token(), &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let claim = *s.claim.lock().unwrap();
    json(
        StatusCode::OK,
        &StatusAnswer {
            state: if claim.is_some() {
                ContainerPhase::Claimed
            } else {
                ContainerPhase::Ready
            },
            user_id: claim.map(|c| c.0),
            generation: claim.map(|c| c.1),
            build: s.build.clone(),
            idle_seconds: 0,
            model: model(),
            stores: vec![],
            opening: vec![],
            locks: vec![],
            snapshot_failures: 0,
            counters: Counters::default(),
        },
    )
}

async fn store_route(State(s): State<Stub>, request: Request) -> Response {
    let headers = request.headers().clone();
    if verify_router_call(&s.launch.router_token(), &headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let user = headers
        .get(headers::USER_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|t| t.parse::<Uuid>().ok())
        .map(UserId::from_uuid);
    if s.claim.lock().unwrap().map(|c| c.0) != user || user.is_none() {
        return json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &serde_json::json!({"error": {"code": "internal"}}),
        );
    }
    s.control.seen.lock().unwrap().push(headers.clone());
    let behaviour = *s.control.behaviour.lock().unwrap();
    match behaviour {
        Behaviour::Echo => {
            let path = request.uri().path().to_owned();
            let method = request.method().to_string();
            let body = axum::body::to_bytes(request.into_body(), 16 << 20)
                .await
                .map(|b| b.len())
                .unwrap_or(0);
            let mut r = json(
                StatusCode::OK,
                &serde_json::json!({ "container": s.id, "path": path, "method": method, "bodyBytes": body }),
            );
            r.headers_mut()
                .insert("x-not-passed", HeaderValue::from_static("1"));
            r
        }
        Behaviour::Hostile => {
            let mut r = json(StatusCode::OK, &serde_json::json!({}));
            let h = r.headers_mut();
            h.insert(
                header::SET_COOKIE,
                HeaderValue::from_static("__Host-session=stolen; Path=/"),
            );
            h.insert(
                header::LOCATION,
                HeaderValue::from_static("https://evil.example/"),
            );
            h.insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=x"),
            );
            r
        }
        Behaviour::Html => {
            let mut r = (StatusCode::OK, "<html></html>").into_response();
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
            r
        }
        Behaviour::Redirect => {
            let mut r = StatusCode::FOUND.into_response();
            r.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_static("https://evil.example/"),
            );
            r
        }
        Behaviour::Unauthorized => json(StatusCode::UNAUTHORIZED, &serde_json::json!({})),
        Behaviour::Parking => {
            let mut r = StatusCode::SERVICE_UNAVAILABLE.into_response();
            r.headers_mut().insert(
                headers::INTERNAL_STATUS,
                HeaderValue::from_static(api::paths::PARKING),
            );
            r
        }
        Behaviour::Hang | Behaviour::Dead => {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }
}

/// A [`Runner`] of stub containers of `build`, all sharing `control`. A stub exits with
/// code 0 when asked to park (unless told not to), and on the launcher's stop.
pub fn stub_runner(build: &str, test_mode: bool, control: Arc<StubControl>) -> Runner {
    let build = build.to_owned();
    Arc::new(
        move |env: ContainerEnv, listener: TcpListener, stop: oneshot::Receiver<()>| {
            let build = build.clone();
            let control = control.clone();
            Box::pin(async move {
                let identity = ServerIdentity::for_container(env.container_id).unwrap();
                let (exit, mut exited) = watch::channel(None);
                let stub = Stub {
                    id: env.container_id,
                    launch: env.launch_secret.clone(),
                    cert_der: Arc::new(identity.cert_der().to_vec()),
                    build,
                    test_mode,
                    control,
                    claim: Arc::new(Mutex::new(None)),
                    exit,
                };
                let mut router = Router::new()
                    .route(api::paths::HELLO, get(hello))
                    .route(api::paths::CLAIM, post(claim))
                    .route(api::paths::PARK, post(park))
                    .route(api::paths::STATUS, get(status));
                for p in crate::router::forward::STORE_ROUTES {
                    router = router.route(p, any(store_route));
                }
                let router = router.with_state(stub);
                let tls = server_config(&identity).unwrap();
                let (done_tx, done_rx) = oneshot::channel::<()>();
                let serve = AbortOnDrop(tokio::spawn(serve_tls(
                    listener,
                    tls,
                    router,
                    |_| Some(()),
                    async {
                        let _ = done_rx.await;
                    },
                )));
                let code = tokio::select! {
                    _ = stop => 0,
                    _ = exited.wait_for(Option::is_some) => 0,
                };
                let _ = done_tx.send(());
                drop(serve);
                code
            })
        },
    )
}

/// Aborts a task when dropped, so that a killed role takes its listener with it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A body of `n` bytes, for requests.
pub fn body_of(n: usize) -> Body {
    Body::from(vec![b'x'; n])
}

/// A state database that fails every write naming a partition that starts with the prefix
/// a test sets in `fail` (`"lease#"`, `"containers"`), as an outage would; everything
/// else goes to `inner`.
#[derive(Debug)]
pub struct FlakyDb {
    inner: Arc<dyn StateDb>,
    /// The partition prefix whose writes fail, if any.
    pub fail: Mutex<Option<&'static str>>,
    /// How many queries (listings) were made.
    pub queries: AtomicUsize,
}

impl FlakyDb {
    /// A database over `inner` that fails nothing yet.
    pub fn new(inner: Arc<dyn StateDb>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail: Mutex::new(None),
            queries: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl StateDb for FlakyDb {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, StateError> {
        self.inner.get(pk, sk).await
    }

    async fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<Item>, StateError> {
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.inner.query(pk, sk_prefix, limit, after).await
    }

    async fn write(&self, writes: Vec<Write>) -> Result<(), StateError> {
        let fail = *self.fail.lock().unwrap();
        if let Some(prefix) = fail
            && writes.iter().any(|w| w.key().0.starts_with(prefix))
        {
            return Err(StateError::Unavailable("a test's fault".into()));
        }
        self.inner.write(writes).await
    }
}
