// SPDX-License-Identifier: AGPL-3.0-only
//! The fleet tests' deployment, all in this process on loopback: one state database in
//! memory, `MemoryStorage` as the bucket, the fake launcher running the real container
//! role (`serve_on`) with the fake model, and routers started through
//! `router::run_router` on loopback listeners, with one host on (the blob route and the
//! peer lists) unless a test turns it off. Requests go over HTTP to a router's public
//! listener. Everything waits in real time, with short limits.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use knoggin_core::{Sensitive, StoreId, Timestamp, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_crypto::testing::test_provider;
use knoggin_http::edge::{AuthMode, EdgeConfig, OauthConfig, Role};
use knoggin_http::testing::issuer::{TEST_SECRET, TestIssuer};
use knoggin_server::container::ServeTimings;
use knoggin_server::launcher::BuildInfo;
use knoggin_server::lease::{Fleet, FleetConfig};
use knoggin_server::link::api::{ClaimLimits, ClaimSettings};
use knoggin_server::link::{
    InternalSecret, LaunchSecret, Peer, RouterClient, ServerIdentity, new_internal_cert,
    router_client,
};
use knoggin_server::router::forward::Timing;
use knoggin_server::router::{Backends, Listeners, RouterStart, reserve, run_router};
use knoggin_server::settings::RouterSettings;
use knoggin_server::storage::MemoryStorage;
use knoggin_server::testing::{FakeLauncher, serve_runner};
use knoggin_state::record::{Container, ContainerState, Leader, Lease};
use knoggin_state::{
    Clock, Cond, Containers, Leases, LocalFileDb, Meta, StateDb, Stores, Users, system_clock,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

/// The waits of every fleet container's park sequence.
pub fn timings() -> ServeTimings {
    ServeTimings {
        drain: Duration::from_millis(500),
        park_retry: Duration::from_millis(500),
    }
}

/// One deployment: what every router and container shares.
pub struct Deployment {
    pub clock: Clock,
    pub db: Arc<dyn StateDb>,
    pub keys: Arc<dyn KeyProvider>,
    pub storage: Arc<MemoryStorage>,
    pub launcher: Arc<FakeLauncher>,
    pub secret: [u8; 32],
    pub cert: String,
    pub key: String,
    pub issuer: Option<TestIssuer>,
}

/// One running router.
pub struct RouterHandle {
    pub url: String,
    pub internal: SocketAddr,
    pub build: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<i32>>,
    pub http: reqwest::Client,
}

impl Drop for RouterHandle {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl RouterHandle {
    /// Asks the router to stop and returns its exit code.
    pub async fn stop(&mut self, limit: Duration) -> i32 {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
        let t = self.task.take().unwrap();
        tokio::time::timeout(limit, t).await.unwrap().unwrap()
    }

    /// A request to the public API.
    pub fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.url))
    }
}

/// What a test changes before a router starts.
pub struct Tweak<'a> {
    pub settings: &'a mut RouterSettings,
    pub edge: &'a mut EdgeConfig,
    pub one_host: &'a mut bool,
    pub timing: &'a mut Timing,
    /// The public listener's address.
    pub public: SocketAddr,
}

impl Deployment {
    /// A deployment with sign-in off (`oauth` false) or on, against the test issuer.
    pub async fn new(oauth: bool) -> Self {
        Self::with_build(oauth, "0.1.0").await
    }

    /// The same, whose launcher starts containers of `build`.
    pub async fn with_build(oauth: bool, build: &str) -> Self {
        let clock = system_clock();
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clone()));
        let storage = Arc::new(MemoryStorage::start(clock.clone()).await.unwrap());
        let launcher = FakeLauncher::new(serve_runner(build, true, timings()), clock.clone());
        let (cert, key) = new_internal_cert(Timestamp::from_millis(clock())).unwrap();
        let issuer = if oauth {
            Some(TestIssuer::start(true).await.unwrap())
        } else {
            None
        };
        let mut secret = [0u8; 32];
        aws_lc_rs::rand::fill(&mut secret).unwrap();
        Self {
            clock,
            db,
            keys: Arc::new(test_provider()),
            storage,
            launcher,
            secret,
            cert,
            key: key.to_string(),
            issuer,
        }
    }

    pub fn leases(&self) -> Leases {
        Leases::new(self.db.clone())
    }

    pub fn containers(&self) -> Containers {
        Containers::new(self.db.clone())
    }

    pub async fn lease(&self, user: UserId) -> Lease {
        self.leases()
            .get(user)
            .await
            .unwrap()
            .map(|l| l.value)
            .unwrap_or_default()
    }

    pub async fn records(&self) -> Vec<Container> {
        self.containers()
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.value)
            .collect()
    }

    pub async fn count(&self, state: ContainerState) -> usize {
        self.records()
            .await
            .iter()
            .filter(|c| c.state == state)
            .count()
    }

    pub async fn users(&self) -> Users {
        let fields = Meta::new(self.db.clone())
            .field_keys(&*self.keys)
            .await
            .unwrap();
        Users::new(self.db.clone(), Arc::new(fields))
    }

    pub async fn stores(&self) -> Stores {
        let fields = Meta::new(self.db.clone())
            .field_keys(&*self.keys)
            .await
            .unwrap();
        Stores::new(self.db.clone(), Arc::new(fields))
    }

    /// The router's settings as a config file would give them.
    pub fn settings(&self, router_url: &str) -> RouterSettings {
        serde_json::from_value(json!({
            "trusted_proxies": ["192.0.2.0/24"],
            "storage": { "bucket": "knoggin-test", "link_ttl_seconds": 300 },
            "containers": {
                "user_net_cidr": "127.0.0.0/8",
                "pool_target": 0,
                "idle_seconds": 600,
                "claim_wait_ms": 15000,
            },
            "internal": {
                "router_url": router_url,
                "secret": base64::engine::general_purpose::STANDARD.encode(self.secret),
                "cert": self.cert,
                "key": self.key,
            },
        }))
        .unwrap()
    }

    /// The edge settings of a router whose public listener is `public`.
    pub fn edge(&self, public: &str) -> EdgeConfig {
        let mut c = EdgeConfig::local("http://127.0.0.1:1".into(), "0.1.0".into());
        c.role = Role::Server;
        c.public_url = Some(public.into());
        c.test_mode = true;
        match &self.issuer {
            None => c.auth_mode = AuthMode::None,
            Some(i) => {
                c.auth_mode = AuthMode::Oauth;
                c.oauth = Some(OauthConfig {
                    issuer: i.url().to_owned(),
                    audiences: vec![],
                    provider_name: "Test IdP".into(),
                    web_client_id: "web-client".into(),
                    web_client_secret: Sensitive::new(TEST_SECRET.into()),
                    admin_emails: vec!["admin@test.example".into()],
                    admin_subjects: vec![],
                    allowed_emails: vec![],
                    allowed_email_domains: vec![],
                    email_claim: None,
                    session_max_days: 7,
                    session_idle_hours: 24,
                });
            }
        }
        c
    }

    /// Starts a router of `build` and waits until its public listener answers.
    pub async fn router(&self, build: &str) -> RouterHandle {
        self.router_with(build, |_| {}).await.unwrap()
    }

    /// The same, after `tweak`; `Err(exit code)` when the router refused to start.
    pub async fn router_with(
        &self,
        build: &str,
        tweak: impl FnOnce(Tweak<'_>),
    ) -> Result<RouterHandle, i32> {
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let public = reserve(loopback).unwrap();
        let internal = reserve(loopback).unwrap();
        let (public_addr, internal_addr) =
            (public.local_addr().unwrap(), internal.local_addr().unwrap());
        let url = format!("http://{public_addr}");
        let router_url = format!("https://127.0.0.1:{}", internal_addr.port());
        let mut settings = self.settings(&router_url);
        let mut edge = self.edge(&url);
        let mut one_host = true;
        let mut timing = Timing {
            silence: Duration::from_secs(2),
            events_silence: Duration::from_secs(3),
            status_limit: Duration::from_secs(1),
            ..Timing::default()
        };
        tweak(Tweak {
            settings: &mut settings,
            edge: &mut edge,
            one_host: &mut one_host,
            timing: &mut timing,
            public: public_addr,
        });
        let start = RouterStart {
            edge,
            settings,
            one_host: one_host.then(|| router_url.clone()),
            build: BuildInfo::new(build),
            spec: if one_host {
                String::new()
            } else {
                format!("td:{build}")
            },
            db: self.db.clone(),
            keys: self.keys.clone(),
            storage: self.storage.clone(),
            launcher: self.launcher.clone(),
            clock: self.clock.clone(),
            listeners: Listeners {
                public,
                internal,
                metrics: None,
            },
            metrics: None,
            before_rollout: None,
            backends: Backends {
                state_db: "local_file",
                key_provider: "key_file",
                launcher: "docker",
            },
            timing,
        };
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run_router(start, async {
            let _ = stopped.await;
        }));
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap();
        let mut handle = RouterHandle {
            url,
            internal: internal_addr,
            build: build.into(),
            stop: Some(stop),
            task: Some(task),
            http,
        };
        // Serving once `/healthz` answers; a refused start returns its code first.
        for _ in 0..200 {
            if handle.task.as_ref().is_some_and(JoinHandle::is_finished) {
                let code = handle.task.take().unwrap().await.unwrap();
                return Err(code);
            }
            if let Ok(r) = handle.req(reqwest::Method::GET, "/healthz").send().await
                && r.status() == 200
            {
                return Ok(handle);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("the router did not start");
    }

    /// A fleet over the same records, for a test to run reconcile passes and "Mark gone"
    /// itself. `build` should be newer than every router's, so that it leads.
    pub async fn fleet(&self, build: &str, one_host: bool, router_url: &str) -> Arc<Fleet> {
        self.fleet_with(build, one_host, router_url, |_| {}).await
    }

    /// The same, after `tweak`.
    pub async fn fleet_with(
        &self,
        build: &str,
        one_host: bool,
        router_url: &str,
        tweak: impl FnOnce(&mut FleetConfig),
    ) -> Arc<Fleet> {
        let settings = self.settings(router_url);
        let fields = Meta::new(self.db.clone())
            .field_keys(&*self.keys)
            .await
            .unwrap();
        let mut config = FleetConfig {
            router_id: Uuid::now_v7(),
            build: BuildInfo::new(build),
            spec: if one_host {
                String::new()
            } else {
                format!("td:{build}")
            },
            one_host,
            test_mode: true,
            containers: settings.containers.clone(),
            claim: ClaimSettings {
                idle_seconds: 600,
                memory_limit_bytes: settings.containers.memory_limit_bytes(),
                public_url: "http://127.0.0.1:1".into(),
                source_url: "https://example.invalid".into(),
                limits: ClaimLimits {
                    body_bytes: 10 << 20,
                    request_timeout_ms: 30_000,
                    store_concurrency: 64,
                },
            },
            secret: InternalSecret::from_bytes(self.secret),
            router_url: router_url.into(),
            router_pin: ServerIdentity::from_pem(&self.cert, &self.key)
                .unwrap()
                .pin(),
            log_level: knoggin_server::launcher::LogLevel::Info,
            allow_downgrade: false,
        };
        tweak(&mut config);
        Fleet::new(
            config,
            self.db.clone(),
            Arc::new(fields),
            self.launcher.clone(),
            self.storage.clone(),
            self.clock.clone(),
        )
        .unwrap()
    }

    /// Makes `fleet` the leader now (the routers take it back on their next renewal).
    pub async fn lead(&self, fleet: &Fleet) {
        let meta = Meta::new(self.db.clone());
        let expired = Leader {
            router_id: "nobody".into(),
            build: "0.0.0".into(),
            until_ms: (self.clock)() - 1,
        };
        meta.put_leader(&expired, Cond::Any).await.unwrap();
        assert!(fleet.take_leadership().await.unwrap());
    }

    /// The launch secret of a container, as only a router can derive it.
    pub fn launch_secret(&self, container: Uuid) -> LaunchSecret {
        InternalSecret::from_bytes(self.secret).launch_secret(container)
    }

    /// A client to the router's internal listener, pinned to the internal certificate,
    /// optionally bound to a local address.
    pub fn broker_client(&self, local: Option<std::net::IpAddr>) -> RouterClient<axum::body::Body> {
        let pin = ServerIdentity::from_pem(&self.cert, &self.key)
            .unwrap()
            .pin();
        router_client(pin, local).unwrap()
    }

    /// An access token of person `sub` for the router at `url`.
    pub fn token(&self, url: &str, sub: &str) -> String {
        let i = self.issuer.as_ref().unwrap();
        i.mint(&i.claims(sub, &format!("{url}/mcp")))
    }

    /// The user's record ID once it exists.
    pub async fn user_of(&self, sub: &str) -> UserId {
        let issuer = self.issuer.as_ref().unwrap().url().to_owned();
        self.users()
            .await
            .find(&issuer, sub)
            .await
            .unwrap()
            .unwrap()
            .value
            .id
    }
}

/// The address and pin of a record.
pub fn peer(c: &Container) -> Peer {
    Peer {
        address: c.address.as_deref().unwrap().parse().unwrap(),
        pin: c.cert_sha256.as_deref().unwrap().parse().unwrap(),
    }
}

/// Waits up to `limit` for `check`, every 50 ms.
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The JSON of an answer.
pub async fn json(r: reqwest::Response) -> (u16, Value) {
    let status = r.status().as_u16();
    let text = r.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// Saves one memory with `auth` (a bearer value, or none); the status and answer.
pub async fn save(r: &RouterHandle, auth: Option<&str>, summary: &str) -> (u16, Value) {
    let mut b = r
        .req(reqwest::Method::POST, "/v1/memories")
        .json(&json!({ "summary": summary }));
    if let Some(a) = auth {
        b = b.bearer_auth(a);
    }
    json(b.send().await.unwrap()).await
}

/// The summaries `GET /v1/memories` lists.
pub async fn summaries(r: &RouterHandle, auth: Option<&str>) -> (u16, Vec<String>) {
    let mut b = r.req(reqwest::Method::GET, "/v1/memories");
    if let Some(a) = auth {
        b = b.bearer_auth(a);
    }
    let (status, v) = json(b.send().await.unwrap()).await;
    let items = v["items"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["summary"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (status, items)
}

/// The one user of a deployment with sign-in off, and their personal store.
pub async fn local_user(d: &Deployment) -> (UserId, StoreId) {
    let u = d
        .users()
        .await
        .find("local", "local")
        .await
        .unwrap()
        .unwrap()
        .value;
    (u.id, u.personal_store_id)
}

/// The log lines written on this thread while it is held: a `#[tokio::test]` runs its
/// tasks on its own thread, so a router started there logs here.
pub struct Logs {
    sink: Arc<std::sync::Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Logs {
    pub fn capture() -> Self {
        let sink = Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || LogWriter(writer.clone()))
            .with_ansi(false)
            .finish();
        Self {
            sink,
            _guard: tracing::subscriber::set_default(subscriber),
        }
    }

    /// What was written since the last call.
    pub fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.sink.lock().unwrap())).unwrap()
    }
}

struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
