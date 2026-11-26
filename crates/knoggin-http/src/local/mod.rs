// SPDX-License-Identifier: AGPL-3.0-only
//! The local daemon's part of the edge (tabs "Sign-in and credentials", The local socket
//! and the local web session, and "Config, CLI, local install", The local daemon): the
//! Unix socket and its directory, the loopback listeners, the local web sessions and
//! their routes, the daemon's versions of the three mode routes, the locked router, and
//! the daemon's erase step.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv6Addr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::{Path as UrlPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use http::{HeaderValue, StatusCode, header};
use knoggin_core::time::{MILLIS_PER_DAY, Timestamp};
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_crypto::token::{hash_secret, random_token};
use knoggin_state::users::LOCAL_IDENTITY;
use knoggin_state::{Clock, EraseReport, NewStore, NewUser, StateDb, StateError, Stores, Users};
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, UnixListener};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::edge::shapes::StoreStatusView;
use crate::edge::{Arrival, Caller, Credential};
use crate::error::{ApiError, Code, LockReason};
use crate::host::{LocalBackend, StoreHost};
use crate::store_api::extract::JsonBody;

/// What every local web session token starts with.
pub const SESSION_PREFIX: &str = "knoggin_session_";

/// A one-time browser code works this many milliseconds, once.
const CODE_TTL_MS: i64 = 60_000;

/// A local web session ends after this many milliseconds without a request.
const SESSION_IDLE_MS: i64 = MILLIS_PER_DAY;

/// The longest socket path the daemon accepts; the system's limit is about 104 bytes.
pub const MAX_SOCKET_PATH: usize = 100;

/// The one-time codes of `knoggin open` and the local web sessions, in memory only: a
/// restart ends every session. Times are the edge's wall clock in milliseconds since 1970,
/// which, unlike a monotonic clock on macOS, keeps counting while the machine sleeps.
#[derive(Debug)]
pub struct LocalSessions {
    origin: String,
    state: Mutex<Held>,
}

#[derive(Debug, Default)]
struct Held {
    /// SHA-256 of a code, and when it expires.
    codes: HashMap<[u8; 32], i64>,
    /// SHA-256 of a session token, and its last request.
    sessions: HashMap<[u8; 32], i64>,
}

impl LocalSessions {
    /// For the local web listener whose origin is `web_origin` (`http://<authority>`).
    pub fn new(web_origin: String) -> Self {
        Self {
            origin: web_origin,
            state: Mutex::new(Held::default()),
        }
    }

    /// This run's web origin.
    pub fn web_origin(&self) -> &str {
        &self.origin
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new one-time code at `now`, valid 60 seconds.
    pub fn new_code(&self, now: i64) -> Result<String, ApiError> {
        let code = random_token().map_err(|_| ApiError::internal())?;
        let mut held = self.held();
        held.codes.retain(|_, until| *until > now);
        held.codes
            .insert(hash_secret(&code), now.saturating_add(CODE_TTL_MS));
        Ok(code)
    }

    /// Spends a code at `now`: a session token, or `None` for a code that is unknown, used
    /// or old.
    pub fn redeem(&self, code: &str, now: i64) -> Result<Option<String>, ApiError> {
        let mut held = self.held();
        match held.codes.remove(&hash_secret(code)) {
            Some(until) if until > now => {}
            _ => return Ok(None),
        }
        let token = format!(
            "{SESSION_PREFIX}{}",
            random_token().map_err(|_| ApiError::internal())?
        );
        held.sessions.insert(hash_secret(&token), now);
        Ok(Some(token))
    }

    /// The session of a whole bearer value at `now`. A request keeps it alive (`touch`);
    /// the 60-second re-check of an open transfer does not.
    pub(crate) fn check(&self, token: &str, now: i64, touch: bool) -> Option<[u8; 32]> {
        let hash = hash_secret(token);
        let mut held = self.held();
        held.sessions
            .retain(|_, seen| now.saturating_sub(*seen) < SESSION_IDLE_MS);
        let seen = held.sessions.get_mut(&hash)?;
        if touch {
            *seen = now;
        }
        Some(hash)
    }

    /// Ends one session.
    pub fn forget(&self, hash: &[u8; 32]) {
        self.held().sessions.remove(hash);
    }

    /// Ends every session but `keep`.
    pub fn forget_all_but(&self, keep: Option<[u8; 32]>) {
        self.held().sessions.retain(|h, _| Some(*h) == keep);
    }
}

/// What the daemon's mode routes read.
#[derive(Clone)]
pub struct LocalState {
    /// The store host.
    pub host: Arc<StoreHost>,
    /// The store records.
    pub stores: Stores,
    /// The local web sessions.
    pub sessions: Arc<LocalSessions>,
    /// `local.data_dir`, for the admin status.
    pub data_dir: PathBuf,
    /// `keychain` or `key_file`.
    pub key_provider: &'static str,
    /// The build's version.
    pub version: String,
    /// Milliseconds since 1970.
    pub clock: Clock,
}

impl std::fmt::Debug for LocalState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalState")
            .field("key_provider", &self.key_provider)
            .finish_non_exhaustive()
    }
}

/// The daemon's mode routes: the store status, the admin status and the snapshot list
/// stub, and the local web session routes.
pub fn mode_routes(state: LocalState) -> Router {
    Router::new()
        .route("/v1/stores/{id}/status", get(store_status))
        .route("/v1/admin/status", get(admin_status))
        .route("/v1/admin/stores/{id}/snapshots", get(snapshots))
        .route("/v1/local/browser-code", post(browser_code))
        .route("/v1/local/session", post(local_session))
        .route("/auth/logout", post(logout))
        .with_state(state)
}

type Answer = Result<Response, ApiError>;

fn ok<T: Serialize>(value: &T) -> Answer {
    let body = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    let mut r = (StatusCode::OK, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    Ok(r)
}

#[utoipa::path(get, path = "/v1/stores/{id}/status", tag = "stores",
    responses((status = 200, body = StoreStatusView), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn store_status(
    State(s): State<LocalState>,
    Extension(c): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
) -> Answer {
    let id = Uuid::parse_str(&id)
        .map(StoreId::from_uuid)
        .map_err(|_| ApiError::invalid_argument("id", "the ID must be a UUID"))?;
    s.stores
        .get_of(c.user.value.id, id)
        .await
        .map_err(crate::edge::state_error)?
        .ok_or_else(|| ApiError::not_found("No such store"))?;
    let status = s.host.store_status(id);
    ok(&StoreStatusView::new(&status, Some(s.host.model_state())))
}

/// The backends of this process.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Backends {
    /// `local_file`, `postgres` or `dynamodb`.
    state_db: &'static str,
    /// `local_dir` or `s3`.
    object_storage: &'static str,
    /// `keychain`, `key_file` or `kms`.
    key_provider: &'static str,
    /// `none`, `docker` or `ecs`.
    launcher: &'static str,
}

/// `{ total, deleting }` users.
#[derive(Serialize, ToSchema)]
pub(crate) struct UserCounts {
    total: u64,
    deleting: u64,
}

/// `GET /v1/admin/status`. On a local install the server-only parts are `null`.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(as = AdminStatus)]
pub(crate) struct AdminStatus {
    version: String,
    image_digest: Option<String>,
    /// `local` or `server`.
    mode: &'static str,
    backends: Backends,
    /// `{ bucket, region }` on a server.
    #[schema(value_type = Option<Object>)]
    storage: Option<serde_json::Value>,
    data_dir: Option<String>,
    /// `{ claimed, ready, starting }` on a server.
    #[schema(value_type = Option<Object>)]
    containers: Option<serde_json::Value>,
    /// `{ target }` on a server.
    #[schema(value_type = Option<Object>)]
    pool: Option<serde_json::Value>,
    /// `{ claimBuild, desiredBuild }` on a server.
    #[schema(value_type = Option<Object>)]
    rollout: Option<serde_json::Value>,
    users: UserCounts,
    retire_before: Option<Timestamp>,
}

#[utoipa::path(get, path = "/v1/admin/status", tag = "admin",
    responses((status = 200, body = AdminStatus), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn admin_status(
    State(s): State<LocalState>,
    Extension(c): Extension<Caller>,
) -> Answer {
    ok(&AdminStatus {
        version: s.version.clone(),
        image_digest: None,
        mode: "local",
        backends: Backends {
            state_db: "local_file",
            object_storage: "local_dir",
            key_provider: s.key_provider,
            launcher: "none",
        },
        storage: None,
        data_dir: Some(s.data_dir.display().to_string()),
        containers: None,
        pool: None,
        rollout: None,
        users: UserCounts {
            total: 1,
            deleting: u64::from(c.user.value.deleted_at_ms.is_some()),
        },
        retire_before: None,
    })
}

#[utoipa::path(get, path = "/v1/admin/stores/{id}/snapshots", tag = "admin",
    responses((status = 404, description = "The snapshot list exists on a server only", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn snapshots() -> Answer {
    Err(ApiError::not_found(
        "The snapshot list exists on a server only",
    ))
}

/// `POST /v1/local/browser-code`'s answer.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(as = BrowserCode)]
pub(crate) struct BrowserCode {
    /// `<web origin>/?t=<code>`.
    url: String,
    expires_at: Timestamp,
}

#[utoipa::path(post, path = "/v1/local/browser-code", tag = "local",
    responses((status = 200, body = BrowserCode), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn browser_code(
    State(s): State<LocalState>,
    Extension(arrival): Extension<Arrival>,
) -> Answer {
    if arrival != Arrival::Socket {
        return Err(ApiError::not_found("No such route"));
    }
    let now = (s.clock)();
    let code = s.sessions.new_code(now)?;
    let expires = now.saturating_add(CODE_TTL_MS);
    ok(&BrowserCode {
        url: format!("{}/?t={code}", s.sessions.web_origin()),
        expires_at: Timestamp::from_millis(expires),
    })
}

/// `POST /v1/local/session`'s body.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = LocalSessionRequest)]
pub(crate) struct CodeBody {
    code: Sensitive<String>,
}

/// `POST /v1/local/session`'s answer.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(as = LocalSession)]
pub(crate) struct SessionToken {
    /// The whole bearer value, `knoggin_session_...`.
    session_token: String,
}

#[utoipa::path(post, path = "/v1/local/session", tag = "local", request_body = CodeBody,
    responses((status = 200, body = SessionToken), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn local_session(
    State(s): State<LocalState>,
    Extension(arrival): Extension<Arrival>,
    JsonBody(body): JsonBody<CodeBody>,
) -> Answer {
    if !matches!(arrival, Arrival::Web { .. }) {
        return Err(ApiError::not_found("No such route"));
    }
    match s.sessions.redeem(body.code.expose(), (s.clock)())? {
        Some(token) => ok(&SessionToken {
            session_token: token,
        }),
        None => Err(ApiError::new(
            Code::Unauthenticated,
            "The code is unknown, used or expired; run knoggin open again",
        )),
    }
}

#[utoipa::path(post, path = "/auth/logout", tag = "local",
    responses((status = 204, description = "The session is forgotten"), (status = "default", body = crate::edge::openapi::ErrorEnvelope)))]
pub(crate) async fn logout(
    State(s): State<LocalState>,
    Extension(arrival): Extension<Arrival>,
    Extension(c): Extension<Caller>,
) -> Answer {
    if !matches!(arrival, Arrival::Web { .. }) {
        return Err(ApiError::not_found("No such route"));
    }
    match c.credential {
        Credential::LocalSession(hash) => {
            s.sessions.forget(&hash);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Credential::Local | Credential::ApiKey(..) | Credential::None => Err(
            ApiError::permission_denied("Logout ends a local web session only"),
        ),
    }
}

/// The one user with issuer and subject `local`, made with its personal store on the first
/// run.
pub async fn ensure_local_user(
    users: &Users,
    keys: &dyn KeyProvider,
    now: i64,
) -> Result<UserId, StateError> {
    if let Some(u) = users.find(LOCAL_IDENTITY, LOCAL_IDENTITY).await? {
        return Ok(u.value.id);
    }
    let store = StoreId::from_uuid(Uuid::now_v7());
    let (_, wrapped) = keys.generate(store).await?;
    let (user, _) = users
        .create(
            NewUser {
                id: UserId::from_uuid(Uuid::now_v7()),
                issuer: LOCAL_IDENTITY.into(),
                subject: Sensitive::new(LOCAL_IDENTITY.into()),
                email: None,
                email_verified: false,
                name: None,
            },
            NewStore {
                id: store,
                name: Sensitive::new("Personal".into()),
                wrapped,
            },
            now,
        )
        .await?;
    Ok(user.value.id)
}

/// The daemon's erase step, now and once an hour: erases stores whose deletion is older
/// than `soft_delete_days` plus 10 minutes, dropping each from memory and deleting its
/// snapshot folder first.
pub async fn erase_step(
    db: Arc<dyn StateDb>,
    host: &StoreHost,
    backend: &LocalBackend,
    now: i64,
    soft_delete_days: u32,
) -> Result<EraseReport, StateError> {
    knoggin_state::erase(db, now, soft_delete_days, |store| {
        host.drop_store(store);
        let dir = backend.store_dir(store);
        async move {
            match tokio::fs::remove_dir_all(&dir).await {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        }
    })
    .await
}

/// Refuses a folder that belongs to another user or that group or others can write.
pub fn check_private_dir(path: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let own = rustix::process::getuid().as_raw();
    if !meta.is_dir() || meta.uid() != own || meta.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be a folder of this user that group and others cannot write",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Checks `<home>`, makes `<home>/run` with mode 0700 if it is missing, and checks it.
pub fn prepare_run_dir(home: &Path) -> io::Result<PathBuf> {
    check_private_dir(home)?;
    let run = home.join("run");
    match std::fs::DirBuilder::new().mode(0o700).create(&run) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    check_private_dir(&run)?;
    Ok(run)
}

/// Binds `<run>/daemon.sock` with mode 0600, replacing a socket file that a crashed
/// daemon left behind (the daemon lock is held, so no daemon uses it). A path longer than
/// 100 bytes is refused.
pub fn bind_socket(run: &Path) -> io::Result<UnixListener> {
    let path = run.join("daemon.sock");
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the socket path {} is longer than {MAX_SOCKET_PATH} bytes; set a shorter KNOGGIN_HOME",
                path.display()
            ),
        ));
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Binds `port` on `127.0.0.1` and on `[::1]`, so that no other account can take the port
/// on the other address. Port 0 lets the system pick one that is free on both. Only a
/// machine without an IPv6 loopback gets the IPv4 listener alone.
pub fn bind_loopback(port: u16) -> io::Result<Vec<TcpListener>> {
    let tokio = |l: std::net::TcpListener| {
        l.set_nonblocking(true)?;
        TcpListener::from_std(l)
    };
    for _ in 0..16 {
        let v4 = std::net::TcpListener::bind(("127.0.0.1", port))?;
        let picked = v4.local_addr()?.port();
        match std::net::TcpListener::bind((Ipv6Addr::LOCALHOST, picked)) {
            Ok(v6) => return Ok(vec![tokio(v4)?, tokio(v6)?]),
            Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => return Ok(vec![tokio(v4)?]),
            Err(e) if port == 0 && e.kind() == io::ErrorKind::AddrInUse => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no port was free on both loopback addresses",
    ))
}

/// This run's label: `k` and the 26 lower-case base32 characters of 16 random bytes.
pub fn web_label() -> Result<String, ApiError> {
    let mut bytes = [0u8; 16];
    use aws_lc_rs::rand::SecureRandom as _;
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| ApiError::internal())?;
    Ok(format!("k{}", base32(&bytes)))
}

/// RFC 4648 base32 in lower case, without padding.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Whether the name form `<label>.localhost` may be used: only on Linux, when the `hosts`
/// line of `nsswitch.conf` lists `myhostname` or `resolve` before `dns`, and the name
/// resolved to loopback addresses only. Never on macOS.
pub fn name_form_allowed(os: &str, nsswitch: Option<&str>, resolved: &[IpAddr]) -> bool {
    if os != "linux" || resolved.is_empty() || !resolved.iter().all(IpAddr::is_loopback) {
        return false;
    }
    let Some(line) = nsswitch.and_then(|text| {
        text.lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix("hosts:"))
    }) else {
        return false;
    };
    let sources: Vec<&str> = line
        .split_whitespace()
        .filter(|w| !w.starts_with('['))
        .collect();
    let local = sources
        .iter()
        .position(|s| *s == "myhostname" || *s == "resolve");
    let dns = sources.iter().position(|s| *s == "dns");
    match (local, dns) {
        (Some(l), Some(d)) => l < d,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The web listener's authority for this run: `<label>.localhost:<port>` where the name
/// form is safe, else `127.0.0.1:<port>`. Reads `/etc/nsswitch.conf` and asks the
/// resolver, so it blocks: call it from a blocking task at start.
pub fn web_authority(label: &str, port: u16) -> String {
    let name = format!("{label}.localhost");
    let allowed = std::env::consts::OS == "linux" && {
        let nsswitch = std::fs::read_to_string("/etc/nsswitch.conf").ok();
        let resolved: Vec<IpAddr> =
            std::net::ToSocketAddrs::to_socket_addrs(&(name.as_str(), port))
                .map(|a| a.map(|s| s.ip()).collect())
                .unwrap_or_default();
        name_form_allowed(std::env::consts::OS, nsswitch.as_deref(), &resolved)
    };
    if allowed {
        format!("{name}:{port}")
    } else {
        format!("127.0.0.1:{port}")
    }
}

/// The daemon in locked mode: `/healthz` and `/readyz` answer as usual, every other route
/// `store_locked` with the reason, and `GET /v1/session` also carries `docsUrl`.
pub fn locked_router(reason: LockReason, version: String, docs_url: String) -> Router {
    let health = version.clone();
    Router::new()
        .route(
            "/healthz",
            get(move || {
                let version = health.clone();
                async move { axum::Json(serde_json::json!({ "status": "ok", "version": version })) }
            }),
        )
        .route(
            "/readyz",
            get(|| async { axum::Json(serde_json::json!({ "status": "ready" })) }),
        )
        .route(
            "/v1/session",
            get(move || {
                let mut e = ApiError::store_locked(reason);
                e.docs_url = Some(docs_url.clone());
                async move { e }
            }),
        )
        .fallback(move || async move { ApiError::store_locked(reason) })
        .layer(middleware::from_fn(locked_entry))
}

async fn locked_entry(request: Request, next: Next) -> Response {
    let id = Uuid::now_v7();
    let response = match request.extensions().get::<Arrival>().cloned() {
        None => ApiError::internal().into_response(),
        Some(arrival) => match crate::edge::browser_check(&request, &arrival) {
            Err(e) => e.into_response(),
            Ok(()) => next.run(request).await,
        },
    };
    crate::edge::finish(response, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base32_matches_rfc_4648() {
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
        assert_eq!(base32(&[0u8; 16]).len(), 26);
        let label = web_label().unwrap();
        assert_eq!(label.len(), 27);
        assert!(label.starts_with('k'));
        assert_ne!(label, web_label().unwrap());
    }

    #[test]
    fn the_name_form_needs_linux_a_local_resolver_and_loopback_answers() {
        let lo = [IpAddr::from([127, 0, 0, 1])];
        let good = "passwd: files\nhosts: files myhostname dns\n";
        assert!(name_form_allowed("linux", Some(good), &lo));
        assert!(name_form_allowed(
            "linux",
            Some("hosts: files resolve [!UNAVAIL=return] dns"),
            &lo
        ));
        assert!(!name_form_allowed(
            "linux",
            Some("hosts: files dns myhostname"),
            &lo
        ));
        assert!(!name_form_allowed("linux", Some("hosts: files dns"), &lo));
        assert!(!name_form_allowed("linux", None, &lo));
        assert!(!name_form_allowed(
            "linux",
            Some(good),
            &[IpAddr::from([10, 0, 0, 1])]
        ));
        assert!(!name_form_allowed("linux", Some(good), &[]));
        // A resolver that answers loopback on a platform outside the rule: the plain form.
        assert!(!name_form_allowed("macos", Some(good), &lo));
    }

    #[test]
    fn codes_work_once_and_expire_and_sessions_idle_out() {
        let s = LocalSessions::new("http://127.0.0.1:1".into());
        let mut now = 1_800_000_000_000;
        let code = s.new_code(now).unwrap();
        let token = s.redeem(&code, now).unwrap().unwrap();
        assert!(token.starts_with(SESSION_PREFIX));
        assert!(s.redeem(&code, now).unwrap().is_none(), "a code works once");
        let late = s.new_code(now).unwrap();
        now += CODE_TTL_MS;
        assert!(s.redeem(&late, now).unwrap().is_none(), "a code expires");
        assert!(s.check(&token, now, true).is_some());
        now += SESSION_IDLE_MS - 1000;
        assert!(
            s.check(&token, now, true).is_some(),
            "a request keeps it alive"
        );
        now += SESSION_IDLE_MS;
        assert!(s.check(&token, now, true).is_none(), "24 idle hours end it");
    }
}
