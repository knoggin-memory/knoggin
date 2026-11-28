// SPDX-License-Identifier: AGPL-3.0-only
//! An in-process rig for the container and the broker: a state database in memory,
//! `MemoryStorage` as the bucket, the broker on a pinned TLS listener on loopback (with a
//! stand-in for the router's "Mark gone" on `parked`), and containers that run the real
//! `serve` role on loopback ports. The test plays the router's part on the link.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::{BodyExt, Empty};
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_http::ctx::{CredentialKind, CredentialRef, Permissions, RequestContext};
use knoggin_server::broker::{BrokerConfig, OnParked, PeerLimits, broker_router};
use knoggin_server::container::{ServeConfig, ServeEnd, ServeModel, ServeTimings, serve_on};
use knoggin_server::launcher::{BuildInfo, ContainerEnv, LogLevel};
use knoggin_server::link::api::{
    ClaimLimits, ClaimRequest, ClaimSettings, HelloAnswer, ParkRequest, StatusAnswer, paths,
};
use knoggin_server::link::{
    CertPin, ForwardClient, InternalSecret, LaunchSecret, Nonce, Peer, Peers, RouterClient,
    ServerIdentity, fetch_unpinned, forwarding_client, new_internal_cert, router_client, serve_tls,
    server_config, write_container_call, write_router_call,
};
use knoggin_server::storage::MemoryStorage;
use knoggin_state::record::{Lease, LeaseState, ParkReason};
use knoggin_state::testing::TestClock;
use knoggin_state::{Cond, Leases, LocalFileDb, NewStore, NewUser, StateDb, Stores, Users};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// How the rig's broker is set up.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Links point at the blob route.
    pub one_host: bool,
    /// `limits.upload_bytes_per_hour`.
    pub upload_bytes_per_hour: u64,
    /// `containers.memory_mib` in bytes, for the broker's size bound.
    pub memory_limit_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            one_host: true,
            upload_bytes_per_hour: u64::MAX,
            memory_limit_bytes: 1 << 30,
        }
    }
}

/// The router's side.
pub struct Rig {
    pub clock: TestClock,
    pub db: Arc<dyn StateDb>,
    pub users: Users,
    pub stores: Stores,
    pub leases: Leases,
    pub keys: Arc<dyn KeyProvider>,
    pub storage: Arc<MemoryStorage>,
    pub secret: Arc<InternalSecret>,
    pub router_pin: CertPin,
    pub router_addr: SocketAddr,
    pub router_url: String,
    /// The path of every broker call, in order.
    pub broker_calls: Arc<Mutex<Vec<String>>>,
    /// The containers that reported `parked`.
    pub parked: Arc<Mutex<Vec<Uuid>>>,
    /// While above zero, each commit is answered 503 and counts it down.
    pub fail_commits: Arc<std::sync::atomic::AtomicUsize>,
    /// One pooled client to the broker, as a container has.
    client: RouterClient<Body>,
    _stop: oneshot::Sender<()>,
}

/// One user with a personal store.
#[derive(Debug, Clone, Copy)]
pub struct Account {
    pub user: UserId,
    pub store: StoreId,
}

impl Rig {
    pub async fn start(options: Options) -> Self {
        let clock = TestClock::new();
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
        let fields = Arc::new(knoggin_state::testing::test_field_keys());
        let users = Users::new(db.clone(), fields.clone());
        let stores = Stores::new(db.clone(), fields);
        let leases = Leases::new(db.clone());
        let keys: Arc<dyn KeyProvider> = Arc::new(knoggin_crypto::testing::test_provider());
        let storage = Arc::new(MemoryStorage::start(clock.clock()).await.unwrap());
        let secret = Arc::new(InternalSecret::from_bytes([7u8; 32]));
        let (cert, key) =
            new_internal_cert(knoggin_core::Timestamp::from_millis(clock.now())).unwrap();
        let identity = ServerIdentity::from_pem(&cert, &key).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let router_addr = listener.local_addr().unwrap();
        let router_url = format!("https://127.0.0.1:{}", router_addr.port());
        let parked = Arc::new(Mutex::new(Vec::new()));
        let on_parked: OnParked = {
            let leases = leases.clone();
            let parked = parked.clone();
            let clock = clock.clone();
            Arc::new(move |caller| {
                let leases = leases.clone();
                let parked = parked.clone();
                let now = clock.now();
                Box::pin(async move {
                    // The records the router's "Mark gone" writes, cut down to the lease.
                    let lease = leases.get(caller.user_id).await.unwrap().unwrap();
                    let mut value = lease.value.clone();
                    value.set_state(LeaseState::Parked, now);
                    leases
                        .put(caller.user_id, &value, lease.cond())
                        .await
                        .unwrap();
                    parked.lock().unwrap().push(caller.container_id);
                    Ok(())
                })
            })
        };
        let peers = PeerLimits::new(clock.clock());
        let config = BrokerConfig {
            secret: secret.clone(),
            leases: leases.clone(),
            stores: stores.clone(),
            keys: keys.clone(),
            storage: Arc::new(storage.without_delete()),
            clock: clock.clock(),
            link_ttl: Duration::from_secs(300),
            memory_limit_bytes: options.memory_limit_bytes,
            upload_bytes_per_hour: options.upload_bytes_per_hour,
            one_host: options.one_host.then(|| router_url.clone()),
            peers: peers.clone(),
            on_parked,
        };
        let broker_calls = Arc::new(Mutex::new(Vec::new()));
        let seen = broker_calls.clone();
        let fail_commits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let failing = fail_commits.clone();
        let router = broker_router(config)
            .unwrap()
            .layer(axum::middleware::from_fn(
                move |req: Request, next: Next| {
                    let path = req.uri().path().to_owned();
                    seen.lock().unwrap().push(path.clone());
                    let fail = path.ends_with("/commit")
                        && failing
                            .try_update(
                                std::sync::atomic::Ordering::SeqCst,
                                std::sync::atomic::Ordering::SeqCst,
                                |n| n.checked_sub(1),
                            )
                            .is_ok();
                    async move {
                        if fail {
                            return axum::response::IntoResponse::into_response(
                                StatusCode::SERVICE_UNAVAILABLE,
                            );
                        }
                        next.run(req).await
                    }
                },
            ));
        let (stop, stopped) = oneshot::channel::<()>();
        tokio::spawn(serve_tls(
            listener,
            server_config(&identity).unwrap(),
            router,
            move |peer| peers.admit(peer),
            async {
                let _ = stopped.await;
            },
        ));
        Self {
            clock,
            db,
            users,
            stores,
            leases,
            keys,
            storage,
            secret,
            router_pin: identity.pin(),
            router_addr,
            router_url,
            broker_calls,
            parked,
            fail_commits,
            client: router_client(identity.pin(), None).unwrap(),
            _stop: stop,
        }
    }

    /// A new user with a personal store.
    pub async fn account(&self) -> Account {
        let user = UserId::from_uuid(Uuid::now_v7());
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = self
            .keys
            .generate(knoggin_crypto::KeyScope {
                store,
                generation: 0,
            })
            .await
            .unwrap();
        self.users
            .create(
                NewUser {
                    id: user,
                    issuer: "https://issuer.test".into(),
                    subject: Sensitive::new(user.to_string()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                self.clock.now(),
            )
            .await
            .unwrap();
        Account { user, store }
    }

    /// Another store (an app) of the user.
    pub async fn app(&self, user: UserId, name: &str) -> StoreId {
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = self
            .keys
            .generate(knoggin_crypto::KeyScope {
                store,
                generation: 0,
            })
            .await
            .unwrap();
        self.stores
            .create_app(
                user,
                NewStore {
                    id: store,
                    name: Sensitive::new(name.into()),
                    wrapped,
                },
                self.clock.now(),
            )
            .await
            .unwrap();
        store
    }

    /// The generation a claim at the rig's time gets.
    pub fn generation(&self) -> u64 {
        u64::try_from(self.clock.now() / 1000).unwrap()
    }

    /// Writes a warm lease of `user` naming `c`, as a claim would.
    pub async fn lease(&self, user: UserId, c: &Container, generation: u64) {
        let old = self.leases.get(user).await.unwrap();
        let mut lease = old.as_ref().map(|l| l.value.clone()).unwrap_or_default();
        lease.generation = generation;
        lease.container_id = Some(c.id);
        lease.address = Some(c.addr.to_string());
        lease.cert_sha256 = c.pin.map(|p| p.to_hex());
        lease.set_state(LeaseState::Warm, self.clock.now());
        let cond = old.map_or(Cond::Absent, |l| l.cond());
        self.leases.put(user, &lease, cond).await.unwrap();
    }

    /// The lease of `user`.
    pub async fn lease_of(&self, user: UserId) -> Lease {
        self.leases.get(user).await.unwrap().unwrap().value
    }

    /// Starts a container role on a loopback port and passes `hello` with it.
    pub async fn container(&self, timings: ServeTimings) -> Container {
        let id = Uuid::now_v7();
        let launch = self.secret.launch_secret(id);
        let env = ContainerEnv {
            container_id: id,
            launch_secret: launch.clone(),
            router_url: self.router_url.clone(),
            router_cert_sha256: self.router_pin,
            log_level: LogLevel::Info,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve_on(
            listener,
            ServeConfig {
                env,
                build: BuildInfo::new("0.1.0"),
                test_mode: true,
                local_address: None,
                model: ServeModel::fake(),
                timings,
            },
            async {
                let _ = stopped.await;
            },
        ));
        let peers = Arc::new(Peers::default());
        let mut c = Container {
            id,
            addr,
            launch,
            pin: None,
            peers: peers.clone(),
            client: forwarding_client(peers).unwrap(),
            task: Some(task),
            stop: Some(stop),
        };
        let nonce = Nonce::random().unwrap();
        let (answer, cert) = c.hello(&nonce).await;
        let answer = answer.expect("hello");
        assert_eq!(answer.container_id, id);
        assert!(c.launch.check_proof(&nonce, &cert, &answer.proof));
        let pin = CertPin::of(&cert);
        c.pin = Some(pin);
        let std::net::SocketAddr::V4(v4) = addr else {
            unreachable!()
        };
        c.peers.set(id, Peer { address: v4, pin });
        c
    }

    /// The rig's pooled client to the broker (a clone shares its connections).
    pub fn broker_client(&self) -> RouterClient<Body> {
        self.client.clone()
    }

    /// A broker call with `id`'s identity (as user `user`), raw.
    pub async fn broker_call(
        &self,
        id: Uuid,
        user: UserId,
        method: Method,
        path: &str,
        body: impl Into<Body>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let launch = self.secret.launch_secret(id);
        let mut req = http::Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.router_url))
            .body(body.into())
            .unwrap();
        write_container_call(req.headers_mut(), &launch, user, id);
        let answer = self.broker_client().request(req).await.unwrap();
        let status = answer.status();
        let headers = answer.headers().clone();
        let bytes = answer.into_body().collect().await.unwrap().to_bytes();
        (status, headers, bytes)
    }
}

/// One running container role, seen from the router.
pub struct Container {
    pub id: Uuid,
    pub addr: SocketAddr,
    pub launch: LaunchSecret,
    pub pin: Option<CertPin>,
    pub peers: Arc<Peers>,
    pub client: ForwardClient<Body>,
    task: Option<JoinHandle<ServeEnd>>,
    stop: Option<oneshot::Sender<()>>,
}

/// The default claim settings with these few changed.
pub fn settings(idle_seconds: u64, memory_limit_bytes: u64) -> ClaimSettings {
    ClaimSettings {
        idle_seconds,
        memory_limit_bytes,
        public_url: "https://knoggin.test".into(),
        source_url: "https://source.test".into(),
        limits: ClaimLimits {
            body_bytes: 10 * 1024 * 1024,
            request_timeout_ms: 30_000,
            store_concurrency: 64,
        },
    }
}

impl Container {
    /// `hello` with `nonce` on a connection of its own: the answer (if 200) and the
    /// certificate of that connection.
    pub async fn hello(&self, nonce: &Nonce) -> (Option<HelloAnswer>, Vec<u8>) {
        let req = http::Request::get(format!("{}?nonce={}", paths::HELLO, nonce.to_hex()))
            .header(http::header::HOST, self.id.to_string())
            .header("knoggin-protocol", "1")
            .body(Empty::<Bytes>::new())
            .unwrap();
        let reply = fetch_unpinned(self.addr, &self.id.to_string(), req)
            .await
            .unwrap();
        let answer =
            (reply.status == StatusCode::OK).then(|| serde_json::from_slice(&reply.body).unwrap());
        (answer, reply.cert_der)
    }

    /// A call to one of the container's routes with the router token (or `token`).
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        body: impl Into<Body>,
        token: Option<&knoggin_server::link::Token>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let mut req = http::Request::builder()
            .method(method)
            .uri(format!("https://{}:{}{path}", self.id, self.addr.port()))
            .body(body.into())
            .unwrap();
        let router = self.launch.router_token();
        write_router_call(req.headers_mut(), token.unwrap_or(&router));
        self.send(req).await
    }

    pub async fn send(&self, req: http::Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let answer = self.client.request(req).await.unwrap();
        let status = answer.status();
        let headers = answer.headers().clone();
        let bytes = answer
            .into_body()
            .collect()
            .await
            .map(|b| b.to_bytes())
            .unwrap_or_default();
        (status, headers, bytes)
    }

    pub async fn claim(
        &self,
        user: UserId,
        generation: u64,
        settings: ClaimSettings,
    ) -> StatusCode {
        let body = serde_json::to_vec(&ClaimRequest {
            user_id: user,
            generation,
            settings,
        })
        .unwrap();
        self.call(Method::POST, paths::CLAIM, body, None).await.0
    }

    pub async fn park(&self, reason: ParkReason) -> StatusCode {
        let body = serde_json::to_vec(&ParkRequest { reason }).unwrap();
        self.call(Method::POST, paths::PARK, body, None).await.0
    }

    pub async fn status(&self) -> StatusAnswer {
        let (status, _, body) = self
            .call(Method::GET, paths::STATUS, Body::empty(), None)
            .await;
        assert_eq!(status, StatusCode::OK);
        serde_json::from_slice(&body).unwrap()
    }

    /// A forwarded store request as `user` for `store`, with read and write.
    pub fn request(
        &self,
        user: UserId,
        store: StoreId,
        method: Method,
        path: &str,
        body: impl Into<Body>,
    ) -> http::Request<Body> {
        let mut req = http::Request::builder()
            .method(method)
            .uri(format!("https://{}:{}{path}", self.id, self.addr.port()))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .unwrap();
        let ctx = RequestContext {
            request_id: Uuid::now_v7(),
            user_id: user,
            store_id: store,
            permissions: Permissions::READ_WRITE,
            credential: CredentialRef {
                kind: CredentialKind::ApiKey,
                id: Some(Uuid::now_v7()),
            },
            default_namespace: None,
            client: None,
        };
        write_router_call(req.headers_mut(), &self.launch.router_token());
        ctx.write_internal_headers(req.headers_mut()).unwrap();
        req
    }

    /// Forwards a store request and reads the whole answer.
    pub async fn forward(
        &self,
        user: UserId,
        store: StoreId,
        method: Method,
        path: &str,
        body: impl Into<Body>,
    ) -> (StatusCode, HeaderMap, Bytes) {
        self.send(self.request(user, store, method, path, body))
            .await
    }

    /// Saves one memory; the status.
    pub async fn save(&self, a: Account, summary: &str) -> StatusCode {
        let body = serde_json::json!({ "summary": summary }).to_string();
        self.forward(a.user, a.store, Method::POST, "/v1/memories", body)
            .await
            .0
    }

    /// Sends the stop signal.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Waits for the role to return, at most `limit`.
    pub async fn end(&mut self, limit: Duration) -> ServeEnd {
        let task = self.task.take().expect("still running");
        tokio::time::timeout(limit, task)
            .await
            .expect("the role ended")
            .unwrap()
    }

    /// Whether the role has returned.
    pub fn ended(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

/// Waits up to `limit` for `check` to hold, looking every 20 ms. The store host cannot
/// run under paused time, so these tests wait in real time with short limits.
pub async fn eventually<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Short park waits for tests.
pub fn quick() -> ServeTimings {
    ServeTimings {
        drain: Duration::from_millis(500),
        park_retry: Duration::from_millis(300),
    }
}

/// The snapshot objects of a store, in order.
pub fn snapshots(rig: &Rig, store: StoreId) -> Vec<String> {
    let prefix = knoggin_server::storage::names::snapshot_prefix(store);
    rig.storage
        .keys()
        .into_iter()
        .filter(|k| k.starts_with(&prefix))
        .collect()
}
