// SPDX-License-Identifier: AGPL-3.0-only
//! The broker: the routes of the router's internal listener that user containers call.
//! Open a store, begin and commit a snapshot, report a park,
//! and on one host the blob route that forwards snapshot bytes to the bucket.
//!
//! Every call proves the container token of the container it names, and must come from
//! the address of the lease that names that container, while the lease is `claiming` or
//! `warm`. Anything else is 409 `lease_lost`. The router mounts [`broker_router`] on its
//! internal listener and admits connections with [`PeerLimits::admit`].

pub mod api;
mod blob;
mod limits;

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::{StoreId, Timestamp, UserId};
use knoggin_crypto::{KeyError, KeyProvider};
use knoggin_http::error::{Code, LockReason};
use knoggin_state::record::{Lease, LeaseState, Newest, ParkReason, StoreRecord};
use knoggin_state::{Clock, Leases, StateError, Stores, Versioned};
use serde::Serialize;
use uuid::Uuid;

pub use limits::{PeerGuard, PeerLimits};

use self::api::{
    BrokerCode, CommitRequest, KeyText, OpenAnswer, OpenRequest, SnapshotLink, SnapshotsRequest,
    UPLOAD_BUDGET, UploadLink, paths,
};
use self::limits::{CallLimits, CallSlot, HourlyBytes};
use crate::json;
use crate::link::{
    CallRefused, ContainerCaller, InternalSecret, LinkError, MAX_ANSWER_BYTES, PeerAddr,
    verify_container_call,
};
use crate::settings::store_budget;
use crate::storage::{ObjectStorage, SignedRequest, StorageError, names};

/// Counts in a commit must not pass this.
const MAX_COUNT: u64 = 1_000_000_000;
/// The longest `lastWriteClient` a commit may carry, in characters.
const MAX_CLIENT_CHARS: usize = 64;

/// What the router runs when a container reports `parked`: "Mark gone" without its stop
/// (how the container ended, then the record `gone` and the lease parked in one write)
/// for that container, then a stop in case it lingers. It belongs to the router's lease
/// code; the broker has checked the caller and its lease first.
pub type OnParked = Arc<
    dyn Fn(ContainerCaller) -> Pin<Box<dyn Future<Output = Result<(), BrokerError>> + Send>>
        + Send
        + Sync,
>;

/// What the broker is built with.
pub struct BrokerConfig {
    /// `internal.secret`: every container token is recomputed from it.
    pub secret: Arc<InternalSecret>,
    /// Lease records.
    pub leases: Leases,
    /// Store records.
    pub stores: Stores,
    /// Unwraps data keys.
    pub keys: Arc<dyn KeyProvider>,
    /// The bucket.
    pub storage: Arc<dyn ObjectStorage>,
    /// The current time.
    pub clock: Clock,
    /// `storage.link_ttl_seconds`.
    pub link_ttl: Duration,
    /// `containers.memory_mib` in bytes: a snapshot may be twice its store budget.
    pub memory_limit_bytes: u64,
    /// `limits.upload_bytes_per_hour` (`RouterSettings::upload_bytes_per_hour`).
    pub upload_bytes_per_hour: u64,
    /// With `one_host`: the router's address as containers reach it (`https://router:8443`
    /// in a deployment). Links then point at the blob route there, and the route is on.
    pub one_host: Option<String>,
    /// The per-address counts that the listener's `admit` also uses.
    pub peers: Arc<PeerLimits>,
    /// What `parked` runs.
    pub on_parked: OnParked,
}

impl std::fmt::Debug for BrokerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerConfig")
            .field("link_ttl", &self.link_ttl)
            .field("memory_limit_bytes", &self.memory_limit_bytes)
            .field("one_host", &self.one_host)
            .finish_non_exhaustive()
    }
}

struct Broker {
    config: BrokerConfig,
    calls: Arc<CallLimits>,
    uploads: HourlyBytes,
    downloads: HourlyBytes,
    bucket: reqwest::Client,
}

impl Broker {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis((self.config.clock)())
    }

    /// The largest snapshot: twice the store budget of a container.
    fn size_bound(&self) -> u64 {
        store_budget(self.config.memory_limit_bytes).saturating_mul(2)
    }

    /// A signed request as the container gets it: on one host the bucket's address and
    /// path are replaced by the blob route on the router; the signed query stays.
    fn link_for(&self, key: &str, signed: SignedRequest) -> (String, BTreeMap<String, String>) {
        let url = match &self.config.one_host {
            Some(router) => {
                let query = signed.url.split_once('?').map_or("", |(_, q)| q);
                format!(
                    "{}{}{key}?{query}",
                    router.trim_end_matches('/'),
                    paths::BLOB_PREFIX
                )
            }
            None => signed.url,
        };
        (url, signed.headers.into_iter().collect())
    }
}

/// An error answer of the broker: the JSON shape of the REST API with a [`BrokerCode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerError {
    /// The code.
    pub code: BrokerCode,
    /// The reason, where one is given.
    pub reason: Option<&'static str>,
}

impl BrokerError {
    fn api(code: Code) -> Self {
        Self {
            code: BrokerCode::Api(code),
            reason: None,
        }
    }

    /// 409 `lease_lost`.
    pub fn lease_lost() -> Self {
        Self {
            code: BrokerCode::LeaseLost,
            reason: None,
        }
    }

    /// 503 `unavailable`: a dependency is down.
    pub fn unavailable() -> Self {
        Self::api(Code::Unavailable)
    }

    /// 500 `internal`.
    pub fn internal() -> Self {
        Self::api(Code::Internal)
    }

    fn invalid() -> Self {
        Self::api(Code::InvalidArgument)
    }

    fn locked(reason: LockReason) -> Self {
        Self {
            code: BrokerCode::Api(Code::StoreLocked),
            reason: Some(reason.as_str()),
        }
    }

    fn status(self) -> StatusCode {
        match self.code {
            BrokerCode::LeaseLost => StatusCode::CONFLICT,
            BrokerCode::Api(code) => code.status(),
        }
    }
}

#[derive(Serialize)]
struct ErrorOut {
    error: ErrorFields,
}

#[derive(Serialize)]
struct ErrorFields {
    code: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl IntoResponse for BrokerError {
    fn into_response(self) -> Response {
        let body = ErrorOut {
            error: ErrorFields {
                code: self.code.as_str(),
                message: "The broker refused the call",
                reason: self.reason,
            },
        };
        let mut response = (
            self.status(),
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            serde_json::to_vec(&body).unwrap_or_default(),
        )
            .into_response();
        if self.code == BrokerCode::Api(Code::Unavailable) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(2));
        }
        response
    }
}

// The errors themselves are not logged: a state error's text can quote a database
// message, a storage error a service code with a key in it.
fn state_failed(_: StateError) -> BrokerError {
    tracing::warn!(kind = "state", "the state database failed in a broker call");
    BrokerError::unavailable()
}

fn storage_failed(_: StorageError) -> BrokerError {
    tracing::warn!(kind = "bucket", "the bucket failed in a broker call");
    BrokerError::unavailable()
}

/// The broker's routes. The router mounts them on its internal listener (after
/// `serve_tls`, which puts [`PeerAddr`] in every request); every other path is 404.
/// Fails only when the HTTP client for the blob route cannot be built.
pub fn broker_router(config: BrokerConfig) -> Result<Router, LinkError> {
    let bucket = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(crate::link::CONNECT_TIMEOUT)
        .build()
        .map_err(|_| LinkError::Tls)?;
    let broker = Arc::new(Broker {
        calls: CallLimits::new(config.clock.clone()),
        uploads: HourlyBytes::new(config.clock.clone(), config.upload_bytes_per_hour),
        downloads: HourlyBytes::new(config.clock.clone(), config.upload_bytes_per_hour),
        bucket,
        config,
    });
    let mut router = Router::new()
        .route(paths::OPEN, post(open))
        .route(paths::SNAPSHOTS, post(snapshots))
        .route(paths::COMMIT, post(commit))
        .route(paths::PARKED, post(parked));
    if broker.config.one_host.is_some() {
        router = router.route(&format!("{}{{*key}}", paths::BLOB_PREFIX), any(blob::blob));
    }
    Ok(router
        .fallback(|| async { BrokerError::api(Code::NotFound) })
        .with_state(broker))
}

/// A checked caller: its identity, its lease, and its call slot.
struct Checked {
    caller: ContainerCaller,
    lease: Versioned<Lease>,
    slot: CallSlot,
}

/// The checks of every call: the peer address is not refused, protocol, container token, the lease (it names this
/// container, is `claiming` or `warm`, and has the caller's IP), then the per-container
/// limits.
async fn check(
    broker: &Broker,
    peer: PeerAddr,
    headers: &HeaderMap,
) -> Result<Checked, BrokerError> {
    // An address refused for failed tokens gets nothing on a connection it already holds.
    if broker.config.peers.is_refused(peer.ip()) {
        return Err(BrokerError::api(Code::RateLimited));
    }
    let caller = match verify_container_call(&broker.config.secret, headers) {
        Ok(caller) => caller,
        Err(CallRefused::Protocol) => return Err(BrokerError::internal()),
        Err(CallRefused::Identity | CallRefused::Token) => {
            broker.config.peers.token_failed(peer.ip());
            return Err(BrokerError::lease_lost());
        }
    };
    let lease = broker
        .config
        .leases
        .get(caller.user_id)
        .await
        .map_err(state_failed)?
        .ok_or_else(BrokerError::lease_lost)?;
    let l = &lease.value;
    let at_address = l
        .address
        .as_deref()
        .and_then(|a| a.parse::<SocketAddr>().ok())
        .is_some_and(|a| a.ip().to_canonical() == peer.ip());
    if l.container_id != Some(caller.container_id)
        || !matches!(l.state, LeaseState::Claiming | LeaseState::Warm)
        || !at_address
    {
        return Err(BrokerError::lease_lost());
    }
    // Only after the lease and the address: a leaked secret used from elsewhere cannot
    // use up the calls of the container it names.
    let slot = broker
        .calls
        .enter(caller.container_id)
        .ok_or_else(|| BrokerError::api(Code::RateLimited))?;
    Ok(Checked {
        caller,
        lease,
        slot,
    })
}

/// The checks of a route that names a store: a UUID, owned by the lease's user, not
/// marked deleted.
async fn store_of(
    broker: &Broker,
    user: UserId,
    store: &str,
) -> Result<Versioned<StoreRecord>, BrokerError> {
    let id = Uuid::parse_str(store).map_err(|_| BrokerError::invalid())?;
    let record = broker
        .config
        .stores
        .get_of(user, StoreId::from_uuid(id))
        .await
        .map_err(state_failed)?
        .ok_or_else(|| BrokerError::api(Code::NotFound))?;
    if record.value.deleted_at_ms.is_some() {
        return Err(BrokerError::api(Code::StoreDeleting));
    }
    Ok(record)
}

/// A JSON body of at most 1 MiB.
async fn body<T: serde::de::DeserializeOwned>(body: Body) -> Result<T, BrokerError> {
    let bytes = axum::body::to_bytes(body, MAX_ANSWER_BYTES)
        .await
        .map_err(|_| BrokerError::invalid())?;
    let text: &[u8] = if bytes.is_empty() { b"{}" } else { &bytes };
    serde_json::from_slice(text).map_err(|_| BrokerError::invalid())
}

fn name_of(n: Newest) -> Option<SnapshotName> {
    SnapshotName::new(n.generation, n.sequence).ok()
}

async fn open(
    State(broker): State<Arc<Broker>>,
    Extension(peer): Extension<PeerAddr>,
    Path(store): Path<String>,
    req: Request,
) -> Result<Response, BrokerError> {
    let (parts, request_body) = req.into_parts();
    let checked = check(&broker, peer, &parts.headers).await?;
    let user = checked.caller.user_id;
    let record = store_of(&broker, user, &store).await?;
    let request: OpenRequest = body(request_body).await?;
    let after = match request.after {
        Some(a) => {
            Some(SnapshotName::new(a.generation, a.sequence).map_err(|_| BrokerError::invalid())?)
        }
        None => None,
    };
    let generation = checked.lease.value.generation;
    let store_id = record.value.id;
    let prefix = names::snapshot_prefix(store_id);
    let mut listed: Vec<(SnapshotName, u64)> = broker
        .config
        .storage
        .list(&prefix)
        .await
        .map_err(storage_failed)?
        .into_iter()
        .filter_map(|o| Some((names::name_in(&prefix, &o.key)?, o.size)))
        .collect();
    listed.sort_unstable_by_key(|(n, _)| *n);
    let unreadable = || BrokerError::locked(LockReason::SnapshotUnreadable);
    // A name above the lease's generation: saving now would put new snapshots below it.
    if let Some((ahead, _)) = listed
        .iter()
        .rev()
        .find(|(n, _)| n.generation() > generation)
    {
        let now_secs = u64::try_from((broker.config.clock)().div_euclid(1000)).unwrap_or(0);
        if now_secs > ahead.generation() {
            request_lifetime_park(&broker, user, checked.lease).await;
        }
        return Err(unreadable());
    }
    let restore_from = record
        .value
        .restore_from
        .as_deref()
        .map(|t| t.parse::<SnapshotName>().map_err(|_| unreadable()))
        .transpose()?;
    let floor = record.value.newest.and_then(name_of);
    let chosen = match restore_from {
        Some(name) => {
            if after.is_some() {
                return Err(unreadable());
            }
            Some(
                listed
                    .iter()
                    .find(|(n, _)| *n == name)
                    .copied()
                    .ok_or_else(unreadable)?,
            )
        }
        None => {
            let found = listed
                .iter()
                .rev()
                .filter(|(n, _)| floor.is_none_or(|f| *n >= f))
                .find(|(n, _)| after.is_none_or(|a| *n < a))
                .copied();
            if found.is_none() && (floor.is_some() || after.is_some()) {
                return Err(unreadable());
            }
            found
        }
    };
    let wrapped = record.value.wrapped().map_err(state_failed)?;
    let key = broker
        .config
        .keys
        .unwrap(store_id, &wrapped)
        .await
        .map_err(|e| match e {
            KeyError::UnknownMasterKey { .. } | KeyError::Unwrap => {
                BrokerError::locked(LockReason::MasterKeyMissing)
            }
            _ => BrokerError::unavailable(),
        })?;
    let next_sequence = listed
        .iter()
        .filter(|(n, _)| n.generation() == generation)
        .map(|(n, _)| n.sequence())
        .max()
        .map_or(1, |s| s + 1);
    let snapshot = match chosen {
        None => None,
        Some((name, size)) => {
            let object = names::snapshot_key(store_id, name);
            let signed = broker
                .config
                .storage
                .presign_get(&object, broker.config.link_ttl, broker.now())
                .await
                .map_err(storage_failed)?;
            let (url, headers) = broker.link_for(&object, signed);
            Some(SnapshotLink {
                generation: name.generation(),
                sequence: name.sequence(),
                size_bytes: size,
                url,
                headers,
            })
        }
    };
    Ok(json(&OpenAnswer {
        generation,
        next_sequence,
        data_key: KeyText(key.to_base64url()),
        wrapped_key: record.value.wrapped_key.clone(),
        kek_id: record.value.kek_id.clone(),
        restore: restore_from.is_some(),
        snapshot,
    }))
}

/// Real time has passed a name above the lease's generation: the next claim must bring a
/// generation above it, so the container is asked to park for `lifetime`. A lost race is
/// harmless: the next open asks again.
async fn request_lifetime_park(broker: &Broker, user: UserId, lease: Versioned<Lease>) {
    let cond = lease.cond();
    let mut value = lease.value;
    if value.request_park(ParkReason::Lifetime, (broker.config.clock)())
        && broker.config.leases.put(user, &value, cond).await.is_err()
    {
        tracing::debug!(user_id = %user, "a lifetime park request lost a race");
    }
}

async fn snapshots(
    State(broker): State<Arc<Broker>>,
    Extension(peer): Extension<PeerAddr>,
    Path(store): Path<String>,
    req: Request,
) -> Result<Response, BrokerError> {
    let (parts, request_body) = req.into_parts();
    let checked = check(&broker, peer, &parts.headers).await?;
    let user = checked.caller.user_id;
    let record = store_of(&broker, user, &store).await?;
    let request: SnapshotsRequest = body(request_body).await?;
    if request.size_bytes > broker.size_bound() {
        return Err(BrokerError::invalid());
    }
    let name = SnapshotName::new(checked.lease.value.generation, request.sequence)
        .map_err(|_| BrokerError::invalid())?;
    if !broker.uploads.take(user, request.size_bytes) {
        return Err(BrokerError {
            code: BrokerCode::Api(Code::RateLimited),
            reason: Some(UPLOAD_BUDGET),
        });
    }
    let object = names::snapshot_key(record.value.id, name);
    let signed = broker
        .config
        .storage
        .presign_put_new(
            &object,
            request.size_bytes,
            broker.config.link_ttl,
            broker.now(),
        )
        .await
        .map_err(storage_failed)?;
    let method = signed.method.clone();
    let expires_at = Timestamp::from_millis(signed.expires_ms)
        .to_rfc3339()
        .ok_or_else(BrokerError::internal)?;
    let (url, headers) = broker.link_for(&object, signed);
    Ok(json(&UploadLink {
        method,
        url,
        headers,
        expires_at,
    }))
}

async fn commit(
    State(broker): State<Arc<Broker>>,
    Extension(peer): Extension<PeerAddr>,
    Path((store, sequence)): Path<(String, u64)>,
    req: Request,
) -> Result<Response, BrokerError> {
    let (parts, request_body) = req.into_parts();
    let checked = check(&broker, peer, &parts.headers).await?;
    let user = checked.caller.user_id;
    let record = store_of(&broker, user, &store).await?;
    let request: CommitRequest = body(request_body).await?;
    let name = SnapshotName::new(checked.lease.value.generation, sequence)
        .map_err(|_| BrokerError::invalid())?;
    if request.memory_count > MAX_COUNT
        || request.namespace_count > MAX_COUNT
        || request
            .last_write_client
            .as_ref()
            .is_some_and(|c| c.chars().count() > MAX_CLIENT_CHARS)
    {
        return Err(BrokerError::invalid());
    }
    let last_write_ms = match &request.last_write_at {
        Some(text) => Some(
            Timestamp::parse_rfc3339(text)
                .map_err(|_| BrokerError::invalid())?
                .millis(),
        ),
        None => None,
    };
    broker
        .config
        .stores
        .update(user, record.value.id, |r| {
            if r.deleted_at_ms.is_some() {
                return Err(CommitError::Deleted);
            }
            // Only ever raise `newest`; a lower name is answered as done.
            if r.newest.and_then(name_of).is_some_and(|n| n >= name) {
                return Ok(());
            }
            r.newest = Some(Newest {
                generation: name.generation(),
                sequence: name.sequence(),
            });
            r.memory_count = request.memory_count;
            r.namespace_count = request.namespace_count;
            if let Some(ms) = last_write_ms {
                r.last_write_ms = Some(ms);
                r.last_write_client = request
                    .last_write_client
                    .clone()
                    .map(knoggin_core::Sensitive::new);
            }
            if request.restored.is_some() && request.restored == r.restore_from {
                r.restore_from = None;
            }
            Ok(())
        })
        .await
        .map_err(|e| match e {
            CommitError::Deleted => BrokerError::api(Code::StoreDeleting),
            CommitError::State(e) => state_failed(e),
        })?;
    Ok(StatusCode::OK.into_response())
}

enum CommitError {
    Deleted,
    State(StateError),
}

impl From<StateError> for CommitError {
    fn from(e: StateError) -> Self {
        Self::State(e)
    }
}

async fn parked(
    State(broker): State<Arc<Broker>>,
    Extension(peer): Extension<PeerAddr>,
    headers: HeaderMap,
) -> Result<Response, BrokerError> {
    let checked = check(&broker, peer, &headers).await?;
    (broker.config.on_parked)(checked.caller).await?;
    Ok(StatusCode::OK.into_response())
}
