// SPDX-License-Identifier: AGPL-3.0-only
//! The local daemon (tab "Config, CLI, local install", The local daemon): its start-up in
//! order, locked mode, the listeners, the background tasks and the shut-down; and the
//! process side of `knoggin start`, `stop` and `state use-previous`.

use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use axum::Router;
use axum::extract::Request;
use knoggin_crypto::{KeyProvider, LocalKeyProvider};
use knoggin_http::edge::listen::{ListenConfig, serve_tcp, serve_unix};
use knoggin_http::edge::{
    self, Arrival, AuthMode as EdgeAuth, EdgeConfig, EdgeParts, EdgeState, Rate,
};
use knoggin_http::host::{EmbedWaits, Embedder, HostConfig, LocalBackend, StoreHost};
use knoggin_http::local::{self, LocalSessions, LocalState};
use knoggin_http::settings::StoreLimits;
use knoggin_http::{LockReason, store_layer};
use knoggin_state::{
    FieldKeys, LocalFileDb, Meta, StateDb, StateError, Stores, Users, system_clock,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower::ServiceExt;

use crate::config::{AuthMode, Config, Env, Home};
use crate::keys::{self, SourceKind};
use crate::model;

/// How long requests in flight may run on after a stop signal.
const DRAIN: Duration = Duration::from_secs(30);
/// While locked, the key source and the state file are tried again this often.
const LOCKED_RETRY: Duration = Duration::from_secs(60);
/// The erase step runs now and this often.
const ERASE_EVERY: Duration = Duration::from_secs(3600);
/// Store records are flushed, and old `open-*.html` files removed, this often.
const MINUTE: Duration = Duration::from_secs(60);

/// What `run/daemon.lock` holds once the listeners are bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LockInfo {
    pub(crate) pid: i32,
    pub(crate) address: String,
}

/// Reads the lock file's content; `None` when it is missing or empty.
pub(crate) fn read_lock_info(home: &Home) -> Option<LockInfo> {
    let text = std::fs::read(home.lock()).ok()?;
    serde_json::from_slice(&text).ok()
}

/// Whether a daemon holds the lock now.
pub(crate) fn is_running(home: &Home) -> bool {
    match OpenOptions::new().read(true).open(home.lock()) {
        Ok(f) => matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)),
        Err(_) => false,
    }
}

/// The checks of start-up step 3 on the settings alone.
pub(crate) fn check_settings(config: &Config, test_mode: bool) -> anyhow::Result<()> {
    let bind = config.bind();
    if !bind.ip().is_loopback() {
        bail!(
            "server.bind {bind} is not a loopback address: the daemon's listener speaks plain \
             HTTP, and a tunnel needs only loopback"
        );
    }
    let public = config.server.public_url.as_deref();
    if let Some(url) = public {
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| anyhow!("server.public_url {url:?} is not a URL"))?;
        let loopback = crate::bridge::loopback_host(&parsed);
        if test_mode && !loopback {
            bail!("test mode refuses a server.public_url that is not a loopback address");
        }
        let https = parsed.scheme() == "https";
        if !https && !(test_mode && loopback && parsed.scheme() == "http") {
            bail!(
                "server.public_url must be https (plain http only in test mode, on a loopback address)"
            );
        }
    }
    if config.auth_mode() == AuthMode::Oauth {
        if config.auth.allowed_emails.is_empty() {
            bail!("with auth.mode = \"oauth\" the daemon needs auth.allowed_emails");
        }
        if !config.auth.allowed_email_domains.is_empty() {
            bail!(
                "the daemon refuses auth.allowed_email_domains: a domain would let in everyone who has an address there"
            );
        }
        if public.is_none() {
            bail!("with auth.mode = \"oauth\" the daemon needs an https server.public_url");
        }
    }
    let data = std::path::absolute(&config.local.data_dir)?;
    let key = std::path::absolute(&config.local.master_key_file)?;
    if key.starts_with(&data) {
        bail!(
            "local.master_key_file {} lies inside local.data_dir; keep the key outside the data folder",
            key.display()
        );
    }
    Ok(())
}

fn make_private_dir(path: &Path) -> anyhow::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    local::check_private_dir(path).map_err(anyhow::Error::from)
}

/// Whether `dir` holds a file whose name passes `keep`, or (with `nested`) a folder that does.
fn has_file(dir: &Path, nested: bool, keep: &dyn Fn(&str) -> bool) -> std::io::Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if nested && entry.file_type()?.is_dir() {
            if has_file(&entry.path(), false, keep)? {
                return Ok(true);
            }
        } else if !nested && keep(&name) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn state_files_exist(data: &Path) -> std::io::Result<bool> {
    has_file(&data.join("state"), false, &|n| {
        n.starts_with("state-") && n.ends_with(".kgs")
    })
}

fn snapshots_exist(data: &Path) -> std::io::Result<bool> {
    has_file(&data.join("stores"), true, &|n| n.ends_with(".kgs"))
}

/// An install whose key and state file could be read.
pub(crate) struct Unlocked {
    pub(crate) db: Arc<LocalFileDb>,
    pub(crate) keys: Arc<LocalKeyProvider>,
    pub(crate) fields: Arc<FieldKeys>,
    pub(crate) source: SourceKind,
}

impl std::fmt::Debug for Unlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unlocked")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Steps 5 and 6: the key source and the state file, or the reason the install is locked.
pub(crate) async fn open_install(
    config: &Config,
    home: &Home,
) -> anyhow::Result<Result<Unlocked, LockReason>> {
    let data = config.local.data_dir.clone();
    let (has_state, has_snapshots) = (state_files_exist(&data)?, snapshots_exist(&data)?);
    let record = keys::read_record(&data).unwrap_or_else(|e| {
        tracing::warn!(kind = "key_source_unreadable", error = %format!("{e:#}"), "key-source.json cannot be read");
        None
    });
    let (c, h) = (config.clone(), home.clone());
    let loaded = tokio::task::spawn_blocking(move || match record {
        None if !has_state && !has_snapshots => {
            let chosen = keys::first_run(&c, &h)?;
            if let Some(why) = &chosen.fell_back {
                tracing::warn!(kind = "keychain_fallback", reason = %why, "the keychain could not be used; the master key is in a key file instead");
            }
            keys::record_choice(&c.local.data_dir, &chosen)?;
            tracing::warn!(
                kind = "first_run",
                source = chosen.source.kind().as_str(),
                "first run: a master key was made. Losing every copy of the master key loses the data; back it up with `knoggin key export`"
            );
            Ok(Some((chosen.source.kind(), chosen.keys)))
        }
        None => Ok(None),
        Some(r) => match keys::load(&c, &h, &r) {
            Ok(k) => Ok(Some((r.source, k))),
            Err(e) => {
                tracing::warn!(kind = "master_key_unreadable", error = %format!("{e:#}"), "the master key cannot be read");
                Ok(None)
            }
        },
    })
    .await
    .map_err(|_| anyhow!("internal error"))?;
    let loaded: Option<_> = loaded.map_err(|e: anyhow::Error| e)?;
    // The key is looked at first.
    let Some((source, master)) = loaded else {
        return Ok(Err(LockReason::MasterKeyMissing));
    };
    if !has_state && has_snapshots {
        return Ok(Err(LockReason::StateMissing));
    }
    let provider = Arc::new(LocalKeyProvider::new(master));
    let state_dir = data.join("state");
    let db = match LocalFileDb::open(&state_dir, provider.as_ref(), system_clock()).await {
        Ok(Some(db)) => db,
        // The first run, or an interrupted one.
        Ok(None) => LocalFileDb::create(&state_dir, provider.as_ref(), system_clock()).await?,
        Err(StateError::Key(_)) => return Ok(Err(LockReason::MasterKeyMissing)),
        Err(e) => {
            tracing::warn!(kind = "state_unreadable", error = %e, "the newest state file cannot be read");
            return Ok(Err(LockReason::StateUnreadable));
        }
    };
    let db = Arc::new(db);
    let meta = Meta::new(db.clone());
    meta.check_schema().await?;
    let fields = Arc::new(meta.field_keys(provider.as_ref()).await?);
    let users = Users::new(db.clone(), fields.clone());
    local::ensure_local_user(&users, provider.as_ref(), now_ms()).await?;
    Ok(Ok(Unlocked {
        db,
        keys: provider,
        fields,
        source,
    }))
}

fn now_ms() -> i64 {
    (system_clock())()
}

/// Step 4: removes what an earlier run left in `run/`.
fn clean_run_dir(run: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(run)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("import-") && entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else if (name.starts_with("open-") && name.ends_with(".html")) || name == "daemon.sock" {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Removes `open-*.html` files older than 60 seconds.
fn remove_old_open_files(run: &Path) {
    let Ok(entries) = std::fs::read_dir(run) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > MINUTE);
        if name.starts_with("open-") && name.ends_with(".html") && old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A router that the listeners keep while what it serves changes: locked mode first, the
/// whole API once the install can be read.
#[derive(Clone)]
struct Swap(Arc<RwLock<Router>>);

impl Swap {
    fn router(&self) -> Router {
        let inner = self.0.clone();
        Router::new().fallback_service(tower::service_fn(move |request: Request| {
            let router = inner.read().unwrap_or_else(PoisonError::into_inner).clone();
            router.oneshot(request)
        }))
    }

    fn set(&self, router: Router) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = router;
    }
}

/// The running install behind the API.
struct Live {
    host: Arc<StoreHost>,
    backend: Arc<LocalBackend>,
    tasks: Vec<JoinHandle<()>>,
    /// Keeps the model state channel open for the daemon's life (see `model::Model`).
    _model_state: watch::Sender<knoggin_core::ops::ModelState>,
}

/// What `go_live` needs besides the install.
struct Wiring {
    config: Config,
    home: Home,
    test_mode: bool,
    ort_dylib: Option<PathBuf>,
    own_url: String,
    sessions: Arc<LocalSessions>,
}

fn edge_config(config: &Config, own_url: String) -> EdgeConfig {
    let l = &config.limits;
    let mut e = EdgeConfig::local(own_url, env!("CARGO_PKG_VERSION").to_owned());
    e.auth_mode = match config.auth_mode() {
        AuthMode::None => EdgeAuth::None,
        AuthMode::Oauth => EdgeAuth::Oauth,
    };
    e.public_url.clone_from(&config.server.public_url);
    e.source_url.clone_from(&config.source_url);
    e.retention_days = config.retention.days;
    e.soft_delete_days = config.retention.soft_delete_days;
    e.per_credential = Rate {
        per_second: l.per_credential_rps,
        burst: l.per_credential_burst,
    };
    e.limits = store_limits(config);
    e
}

fn store_limits(config: &Config) -> StoreLimits {
    let l = &config.limits;
    StoreLimits {
        body_bytes: usize::try_from(l.body_bytes).unwrap_or(usize::MAX),
        request_timeout_ms: l.request_timeout_ms,
        store_concurrency: l.store_concurrency as usize,
    }
}

/// Builds the whole API over an unlocked install and starts its tasks.
async fn go_live(w: &Wiring, u: Unlocked) -> anyhow::Result<(Router, Live)> {
    let config = &w.config;
    let model = if w.test_mode {
        model::fake()
    } else {
        let library = model::library_path(w.ort_dylib.as_deref())?;
        model::start(config.model.dir.clone(), config.model.download, library)
    };
    let mut host_config = HostConfig::local(model.identity.clone());
    host_config.limits = store_limits(config);
    host_config.web_url.clone_from(&config.server.public_url);
    host_config.mcp.source_url.clone_from(&config.source_url);
    let db: Arc<dyn StateDb> = u.db.clone();
    let stores = Stores::new(db.clone(), u.fields.clone());
    let keys: Arc<dyn KeyProvider> = u.keys.clone();
    let backend = Arc::new(LocalBackend::new(
        &config.local.data_dir,
        keys.clone(),
        stores.clone(),
        config.retention.days,
    ));
    let embedder = Embedder::new(
        model.state.clone(),
        model.providers.clone(),
        EmbedWaits::default(),
    );
    let host = StoreHost::new(host_config, backend.clone(), embedder);
    let edge = EdgeState::new(
        edge_config(config, w.own_url.clone()),
        EdgeParts {
            db: db.clone(),
            field_keys: u.fields.clone(),
            keys,
            clock: system_clock(),
            host: Some(host.clone()),
            local: Some(w.sessions.clone()),
        },
    )
    .await?;
    let mode = local::mode_routes(LocalState {
        host: host.clone(),
        stores,
        sessions: w.sessions.clone(),
        data_dir: config.local.data_dir.clone(),
        key_provider: u.source.as_str(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        clock: system_clock(),
    });
    let router = edge::edge_layer(edge, store_layer(host.clone()), mode);

    let mut tasks = Vec::new();
    tasks.extend(model.task);
    let (h, b, d) = (host.clone(), backend.clone(), db.clone());
    let soft = config.retention.soft_delete_days;
    tasks.push(tokio::spawn(async move {
        let mut every = tokio::time::interval(ERASE_EVERY);
        loop {
            every.tick().await;
            match local::erase_step(d.clone(), &h, &b, now_ms(), soft).await {
                Ok(report) => tracing::info!(kind = "erase", report = ?report, "erase step done"),
                Err(e) => {
                    tracing::warn!(kind = "erase_failed", error = %e, "the erase step failed")
                }
            }
        }
    }));
    let (b, run) = (backend.clone(), w.home.run());
    tasks.push(tokio::spawn(async move {
        let mut every = tokio::time::interval(MINUTE);
        loop {
            every.tick().await;
            if let Err(e) = b.flush_records().await {
                tracing::warn!(kind = "flush_failed", error = %e, "the store records could not be written");
            }
            let r = run.clone();
            let _ = tokio::task::spawn_blocking(move || remove_old_open_files(&r)).await;
        }
    }));
    Ok((
        router,
        Live {
            host,
            backend,
            tasks,
            _model_state: model.sender,
        },
    ))
}

fn docs_url(config: &Config) -> String {
    format!(
        "{}/blob/v{}/docs",
        config.source_url,
        env!("CARGO_PKG_VERSION")
    )
}

/// Binds the fixed listener: on `127.0.0.1` the same port on `[::1]` as well.
fn bind_fixed(addr: SocketAddr) -> anyhow::Result<Vec<TcpListener>> {
    let taken = |e: std::io::Error| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            anyhow!(
                "port {} is taken by another program; stop it, or set server.bind to another loopback address and port",
                addr.port()
            )
        } else {
            anyhow!("binding {addr} (server.bind): {e}")
        }
    };
    if addr.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) {
        local::bind_loopback(addr.port()).map_err(taken)
    } else {
        let l = std::net::TcpListener::bind(addr).map_err(taken)?;
        l.set_nonblocking(true)?;
        Ok(vec![TcpListener::from_std(l)?])
    }
}

/// Runs the daemon until a stop signal. Returns the exit code: 0, or 1 when a final
/// snapshot could not be written.
pub(crate) async fn run(config: Config, env: &Env) -> anyhow::Result<u8> {
    let home = env.paths();
    if env.test_mode {
        tracing::warn!(
            kind = "test_mode",
            "test mode is on: the fake embedding model is used"
        );
    }
    // Step 2: the lock, held for the daemon's life.
    make_private_dir(home.root())?;
    let run_dir = local::prepare_run_dir(home.root())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(home.lock())
        .with_context(|| format!("opening {}", home.lock().display()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => bail!("Knoggin is already running"),
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("taking the daemon lock"),
    }
    lock.set_len(0)?;
    // Step 3.
    check_settings(&config, env.test_mode)?;
    if home.socket().as_os_str().len() > local::MAX_SOCKET_PATH {
        bail!(
            "the socket path {} is longer than {} bytes; set a shorter KNOGGIN_HOME",
            home.socket().display(),
            local::MAX_SOCKET_PATH
        );
    }
    let data = &config.local.data_dir;
    for dir in [data.clone(), data.join("state"), data.join("stores")] {
        make_private_dir(&dir)?;
    }
    if let Some(dir) = config.local.master_key_file.parent()
        && dir.exists()
    {
        local::check_private_dir(dir)?;
    }
    // Step 4.
    clean_run_dir(&run_dir)?;
    // Steps 5 and 6.
    let opened = open_install(&config, &home).await?;

    // Step 8: the listeners.
    let fixed = bind_fixed(config.bind())?;
    let fixed_addr = fixed
        .first()
        .ok_or_else(|| anyhow!("no fixed listener"))?
        .local_addr()?;
    let web = local::bind_loopback(0).context("binding the local web listener")?;
    let web_port = web
        .first()
        .ok_or_else(|| anyhow!("no web listener"))?
        .local_addr()?
        .port();
    // The socket last: a daemon that cannot take its port must not be found on it.
    let socket = local::bind_socket(&run_dir)?;
    let label = local::web_label().map_err(|_| anyhow!("no random bytes for the web label"))?;
    let authority = tokio::task::spawn_blocking(move || local::web_authority(&label, web_port))
        .await
        .map_err(|_| anyhow!("internal error"))?;
    let wiring = Wiring {
        own_url: format!("http://{fixed_addr}"),
        sessions: Arc::new(LocalSessions::new(format!("http://{authority}"))),
        config: config.clone(),
        home: home.clone(),
        test_mode: env.test_mode,
        ort_dylib: env.ort_dylib.clone(),
    };

    let swap = Swap(Arc::new(RwLock::new(Router::new())));
    let live: Arc<tokio::sync::Mutex<Option<Live>>> = Arc::new(tokio::sync::Mutex::new(None));
    let mut retry = None;
    match opened {
        Ok(u) => {
            let (router, l) = go_live(&wiring, u).await?;
            swap.set(router);
            *live.lock().await = Some(l);
        }
        Err(reason) => {
            tracing::warn!(
                kind = "locked",
                reason = reason.as_str(),
                "the install is locked; `knoggin status` says what to do"
            );
            swap.set(local::locked_router(
                reason,
                env!("CARGO_PKG_VERSION").into(),
                docs_url(&config),
            ));
            let (swap, live, wiring) = (swap.clone(), live.clone(), Arc::new(wiring));
            retry = Some(tokio::spawn(async move {
                loop {
                    tokio::time::sleep(LOCKED_RETRY).await;
                    match open_install(&wiring.config, &wiring.home).await {
                        Ok(Ok(u)) => match go_live(&wiring, u).await {
                            Ok((router, l)) => {
                                swap.set(router);
                                *live.lock().await = Some(l);
                                tracing::info!(kind = "unlocked", "the install can be read again");
                                return;
                            }
                            Err(e) => {
                                tracing::warn!(kind = "unlock_failed", error = %format!("{e:#}"), "starting after the lock failed")
                            }
                        },
                        Ok(Err(_)) => {}
                        Err(e) => {
                            tracing::warn!(kind = "unlock_failed", error = %format!("{e:#}"), "the install could not be opened")
                        }
                    }
                }
            }));
        }
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let stopped = move || {
        let mut rx = stop_rx.clone();
        async move {
            let _ = rx.wait_for(|s| *s).await;
        }
    };
    let router = swap.router();
    let mut listeners = Vec::new();
    listeners.push(tokio::spawn(serve_unix(
        socket,
        router.clone(),
        ListenConfig::DEFAULT,
        stopped(),
    )));
    for l in fixed {
        let arrival = Arrival::Fixed {
            port: fixed_addr.port(),
        };
        listeners.push(tokio::spawn(serve_tcp(
            l,
            router.clone(),
            arrival,
            ListenConfig::DEFAULT,
            stopped(),
        )));
    }
    for l in web {
        let arrival = Arrival::Web {
            authority: authority.clone(),
        };
        listeners.push(tokio::spawn(serve_tcp(
            l,
            router.clone(),
            arrival,
            ListenConfig::DEFAULT,
            stopped(),
        )));
    }
    let mut upkeep = None;
    if let Some(addr) = config.metrics_bind() {
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .context("installing the metrics recorder")?;
        let l = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding metrics.bind {addr}"))?;
        let h = handle.clone();
        upkeep = Some(tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(5));
            loop {
                every.tick().await;
                h.run_upkeep();
            }
        }));
        let arrival = Arrival::Fixed { port: addr.port() };
        listeners.push(tokio::spawn(serve_tcp(
            l,
            edge::metrics_router(handle),
            arrival,
            ListenConfig::DEFAULT,
            stopped(),
        )));
    }
    let info = LockInfo {
        pid: i32::try_from(std::process::id()).unwrap_or(0),
        address: fixed_addr.to_string(),
    };
    (&lock).write_all(&serde_json::to_vec(&info)?)?;
    lock.sync_all()?;
    tracing::info!(
        kind = "listening",
        port = fixed_addr.port(),
        "the daemon is ready"
    );

    wait_for_stop().await;
    tracing::info!(
        kind = "stopping",
        "stop signal: finishing requests in flight"
    );
    let _ = stop_tx.send(true);
    let drain = async {
        for l in &mut listeners {
            let _ = l.await;
        }
    };
    if tokio::time::timeout(DRAIN, drain).await.is_err() {
        tracing::warn!(
            kind = "drain_timeout",
            "requests still in flight after 30 seconds were cut off"
        );
    }
    for l in &listeners {
        l.abort();
    }
    if let Some(t) = retry {
        t.abort();
    }
    if let Some(t) = upkeep {
        t.abort();
    }
    let mut code = 0;
    if let Some(l) = live.lock().await.take() {
        code = shut_down(l).await;
    }
    let _ = std::fs::remove_file(home.socket());
    drop(lock);
    Ok(code)
}

/// Final snapshots (3 tries in 10 seconds), the host's tasks, the store records.
async fn shut_down(live: Live) -> u8 {
    for t in &live.tasks {
        t.abort();
    }
    let mut code = 0;
    for attempt in 1..=3 {
        let failed = live.host.save_all().await;
        if failed.is_empty() {
            break;
        }
        if attempt == 3 {
            for (store, e) in &failed {
                tracing::error!(kind = "final_snapshot_failed", store = %store, error = e.kind(), "a final snapshot could not be written; changes since the last one are lost");
            }
            code = 1;
        } else {
            tokio::time::sleep(Duration::from_secs(4)).await;
        }
    }
    live.host.shutdown();
    if let Err(e) = live.backend.flush_records().await {
        tracing::warn!(kind = "flush_failed", error = %e, "the store records could not be written");
    }
    code
}

async fn wait_for_stop() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // Without signal handlers the daemon could never stop cleanly; run until killed.
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Starts `knoggin daemon` detached: a child in a session of its own, standard input,
/// output and error on `/dev/null`. Returns the child; the caller waits for `/healthz`.
pub(crate) fn spawn_detached(config: Option<&Path>) -> anyhow::Result<std::process::Child> {
    let exe = std::env::current_exe().context("finding the knoggin binary")?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    cmd.arg("daemon")
        .arg("--new-session")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().context("starting the daemon")
}

/// `knoggin stop`: sends the stop signal to the PID in the lock file and waits up to 60
/// seconds for the lock to be released. `false` when no daemon runs.
pub(crate) async fn stop(home: &Home) -> anyhow::Result<bool> {
    if !is_running(home) {
        return Ok(false);
    }
    let info = read_lock_info(home)
        .ok_or_else(|| anyhow!("the lock file holds no process ID yet; try again"))?;
    let pid = rustix::process::Pid::from_raw(info.pid)
        .ok_or_else(|| anyhow!("the lock file holds a bad process ID"))?;
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)
        .map_err(|e| anyhow!("could not signal the daemon: {e}"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while is_running(home) {
        if tokio::time::Instant::now() > deadline {
            bail!("the daemon did not stop within 60 seconds");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(true)
}

/// What `knoggin state use-previous` would do.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Previous {
    pub(crate) file: String,
    pub(crate) written_at: String,
    pub(crate) stores: usize,
    pub(crate) keys: usize,
    /// Store folders that the previous file has no record of.
    pub(crate) unrecorded_stores: Vec<String>,
}

/// Reads the previous state file, for the question before `use_previous`.
pub(crate) async fn previous(config: &Config, home: &Home) -> anyhow::Result<Option<Previous>> {
    let record = keys::read_record(&config.local.data_dir)?
        .ok_or_else(|| anyhow!("no master key is recorded; nothing to fall back to"))?;
    let (c, h) = (config.clone(), home.clone());
    let master = tokio::task::spawn_blocking(move || keys::load(&c, &h, &record))
        .await
        .map_err(|_| anyhow!("internal error"))??;
    let provider = LocalKeyProvider::new(master);
    let state_dir = config.local.data_dir.join("state");
    let Some(s) = knoggin_state::local::previous_state_file(&state_dir, &provider).await? else {
        return Ok(None);
    };
    let known: Vec<String> = s.store_ids.iter().map(ToString::to_string).collect();
    let mut unrecorded = Vec::new();
    if let Ok(entries) = std::fs::read_dir(config.local.data_dir.join("stores")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !known.contains(&name) {
                unrecorded.push(name);
            }
        }
    }
    unrecorded.sort();
    Ok(Some(Previous {
        file: s.path.display().to_string(),
        written_at: knoggin_core::Timestamp::from_millis(i64::try_from(s.created_ms).unwrap_or(0))
            .to_string(),
        stores: s.store_ids.len(),
        keys: s.key_count,
        unrecorded_stores: unrecorded,
    }))
}

/// Renames the newest state file to `<name>.bad`, with the daemon stopped.
pub(crate) async fn use_previous(config: &Config, home: &Home) -> anyhow::Result<Option<PathBuf>> {
    let h = home.clone();
    let _lock = tokio::task::spawn_blocking(move || keys::hold_daemon_lock(&h))
        .await
        .map_err(|_| anyhow!("internal error"))??;
    Ok(knoggin_state::local::set_aside_newest(&config.local.data_dir.join("state")).await?)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::config::{Flags, Role};

    fn config(vars: &[(&str, &str)]) -> Config {
        let mut all: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        all.insert("HOME".into(), "/nonexistent-home".into());
        all.insert("KNOGGIN_HOME".into(), "/nonexistent-home/k".into());
        let env = Env::from_vars(all, "/".into()).unwrap();
        Config::load(&env, Role::Local, None, &Flags::default()).unwrap()
    }

    #[test]
    fn start_up_refuses_what_step_3_refuses() {
        assert!(check_settings(&config(&[]), false).is_ok());
        for mode in ["none", "oauth"] {
            let c = config(&[
                ("KNOGGIN_SERVER__BIND", "0.0.0.0:7690"),
                ("KNOGGIN_AUTH__MODE", mode),
            ]);
            assert!(
                check_settings(&c, false)
                    .unwrap_err()
                    .to_string()
                    .contains("loopback"),
                "{mode}"
            );
        }
        let oauth = [
            ("KNOGGIN_AUTH__MODE", "oauth"),
            ("KNOGGIN_SERVER__PUBLIC_URL", "https://t.example"),
        ];
        assert!(
            check_settings(&config(&oauth), false)
                .unwrap_err()
                .to_string()
                .contains("allowed_emails")
        );
        let mut with = oauth.to_vec();
        with.push(("KNOGGIN_AUTH__ALLOWED_EMAILS", "me@example.com"));
        assert!(check_settings(&config(&with), false).is_ok());
        with.push(("KNOGGIN_AUTH__ALLOWED_EMAIL_DOMAINS", "example.com"));
        assert!(check_settings(&config(&with), false).is_err());
        let http = [("KNOGGIN_SERVER__PUBLIC_URL", "http://127.0.0.1:7690")];
        assert!(check_settings(&config(&http), false).is_err());
        assert!(check_settings(&config(&http), true).is_ok());
        // Test mode with a public address that is not loopback refuses to start.
        let public = [("KNOGGIN_SERVER__PUBLIC_URL", "https://memory.example.com")];
        assert!(check_settings(&config(&public), false).is_ok());
        assert!(check_settings(&config(&public), true).is_err());
        let inside = [(
            "KNOGGIN_LOCAL__MASTER_KEY_FILE",
            "/nonexistent-home/k/data/master.key",
        )];
        assert!(
            check_settings(&config(&inside), false)
                .unwrap_err()
                .to_string()
                .contains("inside")
        );
    }
}
