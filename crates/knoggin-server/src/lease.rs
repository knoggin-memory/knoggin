// SPDX-License-Identifier: AGPL-3.0-only
//! Leases and the containers they name (tab "Server", States and How a store request is
//! served): [`Fleet`] is what one router knows of the fleet, shared by the request path,
//! the pool and the reconcile loop. It claims a container for a user (with the generation
//! rule and the cap on new containers), waits for a container within the request's
//! deadline, finishes a claim as an owned task, and runs "Mark gone".
//!
//! Every lease change is a conditional write on the record's version, and every write
//! that ties a lease to a container changes both records in one write.

use std::collections::HashMap;
use std::net::SocketAddrV4;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::{BodyExt, Limited};
use knoggin_core::{StoreId, UserId};
use knoggin_http::ApiError;
use knoggin_http::Code;
use knoggin_state::record::{
    BuildSpec, Container, ContainerState, EndKind, Lease, LeaseState, ParkReason, StopReason,
    cond_of,
};
use knoggin_state::{
    Clock, Containers, FieldKeys, Leases, Meta, StateDb, StateError, Stores, Versioned,
};
use serde::Serialize;
use tokio::task::JoinSet;
use tokio::time::Instant;
use uuid::Uuid;

use crate::broker::PeerLimits;
use crate::launcher::{BuildInfo, ContainerInfo, LaunchError, LaunchHandle, Launcher, LogLevel};
use crate::link::api::{self, ClaimRequest, ClaimSettings, ParkRequest, StatusAnswer};
use crate::link::{
    CertPin, ForwardClient, InternalSecret, MAX_ANSWER_BYTES, Peer, Peers, forwarding_client,
    fresh_client, write_router_call,
};
use crate::pool::LaunchGate;
use crate::settings::ContainerSettings;
use crate::storage::ObjectStorage;

/// How long a request waits between two looks at a lease that is not ready.
pub const POLL: Duration = Duration::from_millis(250);

/// The time limit of a `claim` call.
pub const CLAIM_LIMIT: Duration = Duration::from_secs(10);

/// What a router needs to run its part of the fleet, beside the state database and the
/// launcher.
#[derive(Debug, Clone)]
pub struct FleetConfig {
    /// This router's ID, a UUIDv7 made when the process starts.
    pub router_id: Uuid,
    /// This router's build.
    pub build: BuildInfo,
    /// This router's own spec (`launcher.ecs.task_definition`; empty on one host).
    pub spec: String,
    /// One host: the claim build is the router's own, and a container the launcher no
    /// longer knows has ended.
    pub one_host: bool,
    /// Whether this router runs in test mode; a container whose `hello` differs is stopped.
    pub test_mode: bool,
    /// `containers.*`.
    pub containers: ContainerSettings,
    /// What every claim tells its container.
    pub claim: ClaimSettings,
    /// `internal.secret`.
    pub secret: InternalSecret,
    /// `KNOGGIN_ROUTER_URL` for new containers (`internal.router_url`; on one host the
    /// launcher sets its own).
    pub router_url: String,
    /// The pin of the routers' internal certificate.
    pub router_pin: CertPin,
    /// `KNOGGIN_LOG__LEVEL` for new containers.
    pub log_level: LogLevel,
    /// `rollout.allow_downgrade`.
    pub allow_downgrade: bool,
}

/// What one router knows of the fleet: the state database's leases and container
/// records, the launcher, the pinned clients to containers and the router's own tasks.
pub struct Fleet {
    pub(crate) config: FleetConfig,
    pub(crate) db: Arc<dyn StateDb>,
    pub(crate) leases: Leases,
    pub(crate) containers: Containers,
    pub(crate) meta: Meta,
    pub(crate) stores: Stores,
    pub(crate) launcher: Arc<dyn Launcher>,
    pub(crate) storage: Arc<dyn ObjectStorage>,
    pub(crate) peers: Arc<Peers>,
    pub(crate) client: ForwardClient<Body>,
    fresh: ForwardClient<Body>,
    pub(crate) clock: Clock,
    tasks: Mutex<JoinSet<()>>,
    pub(crate) gate: Mutex<LaunchGate>,
    pub(crate) limits: Arc<PeerLimits>,
    /// Request bodies buffered for a resend: all of them and per user.
    pub(crate) buffered: Mutex<(u64, HashMap<UserId, u64>)>,
}

impl std::fmt::Debug for Fleet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fleet")
            .field("router_id", &self.config.router_id)
            .field("build", &self.config.build)
            .finish_non_exhaustive()
    }
}

/// What failed in "Mark gone". Nothing was freed.
#[derive(Debug, thiserror::Error)]
pub enum GoneError {
    /// The launcher could not be reached or the stop failed.
    #[error("the launcher: {0}")]
    Launcher(#[from] LaunchError),
    /// The state database failed.
    #[error("the state database: {0}")]
    State(#[from] StateError),
}

/// Where "Mark gone" stands after one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marked {
    /// The record is `gone` and the lease that named it is parked.
    Gone,
    /// The container still runs (a stop was asked for, or none was wanted); nothing freed.
    Running,
    /// The launcher does not know the container and only the reconcile loop may decide.
    Unknown,
}

/// Who runs "Mark gone".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// A request, a claim or a launch.
    Request,
    /// The reconcile loop; `unseen` is true when the launcher's list has not shown the
    /// container on two passes in a row.
    Reconcile {
        /// Missed by two lists in a row.
        unseen: bool,
    },
}

/// How a container ended (step 2 of "Mark gone").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// It parked itself (exit code 0).
    Parked,
    /// A router stopped it for a reason other than `unresponsive`.
    Stopped,
    /// Killed for memory.
    Memory,
    /// Any other end.
    Failed,
}

impl End {
    fn abnormal(self) -> Option<EndKind> {
        match self {
            Self::Memory => Some(EndKind::Memory),
            Self::Failed => Some(EndKind::Failed),
            Self::Parked | Self::Stopped => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Parked => "parked",
            Self::Stopped => "stopped",
            Self::Memory => "memory",
            Self::Failed => "failed",
        }
    }

    /// The end that a record's `stop_reason` gives, whoever finishes the routine.
    fn of_reason(reason: StopReason) -> Self {
        if reason == StopReason::Unresponsive {
            Self::Failed
        } else {
            Self::Stopped
        }
    }

    /// The end the launcher's answer gives, without a `stop_reason`: the exit code decides.
    fn of_info(info: &ContainerInfo) -> Self {
        match info.ended {
            Some(e) if e.reason == crate::launcher::EndReason::Memory => Self::Memory,
            Some(e) if e.exit_code == Some(0) => Self::Parked,
            _ => Self::Failed,
        }
    }
}

/// What a failed call to a container was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallFailed {
    /// No answer, a broken link, or an answer too large or too slow.
    Link,
}

/// What the request loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Read the lease again at once.
    Again,
    /// Wait 250 ms, then read again.
    Wait,
}

/// `stop_reason` as written in logs and to the launcher.
pub(crate) fn reason_str(r: StopReason) -> &'static str {
    match r {
        StopReason::Unresponsive => "unresponsive",
        StopReason::ParkTimeout => "park_timeout",
        StopReason::Lifetime => "lifetime",
        StopReason::Drain => "drain",
        StopReason::Rollout => "rollout",
        StopReason::Surplus => "surplus",
        StopReason::Stray => "stray",
        StopReason::ClaimFailed => "claim_failed",
        StopReason::LaunchFailed => "launch_failed",
    }
}

/// The state database's failures on the request path: `unavailable`.
pub(crate) fn db_error(e: StateError) -> ApiError {
    tracing::warn!(kind = "state_db", error = %e, "the state database failed");
    ApiError::unavailable(2)
}

/// The address and pin a record names, once its `hello` was checked.
pub(crate) fn peer_of(address: Option<&str>, pin: Option<&str>) -> Option<Peer> {
    Some(Peer {
        address: address?.parse::<SocketAddrV4>().ok()?,
        pin: pin?.parse().ok()?,
    })
}

impl Fleet {
    /// The fleet of one router.
    pub fn new(
        config: FleetConfig,
        db: Arc<dyn StateDb>,
        field_keys: Arc<FieldKeys>,
        launcher: Arc<dyn Launcher>,
        storage: Arc<dyn ObjectStorage>,
        clock: Clock,
    ) -> Result<Arc<Self>, crate::link::LinkError> {
        let peers = Arc::new(Peers::default());
        let limits = PeerLimits::new(clock.clone());
        Ok(Arc::new(Self {
            client: forwarding_client(peers.clone())?,
            fresh: fresh_client(peers.clone())?,
            peers,
            leases: Leases::new(db.clone()),
            containers: Containers::new(db.clone()),
            meta: Meta::new(db.clone()),
            stores: Stores::new(db.clone(), field_keys),
            db,
            launcher,
            storage,
            clock,
            tasks: Mutex::new(JoinSet::new()),
            gate: Mutex::new(LaunchGate::default()),
            limits,
            buffered: Mutex::new((0, HashMap::new())),
            config,
        }))
    }

    /// The settings.
    pub fn config(&self) -> &FleetConfig {
        &self.config
    }

    /// The internal listener's counts per peer address, shared by the listener's `admit`
    /// and the broker's token checks.
    pub fn limits(&self) -> &Arc<PeerLimits> {
        &self.limits
    }

    /// Milliseconds since 1970.
    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Runs `task` as one of the router's own tasks, which outlive the request that
    /// started them and end at shutdown.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = crate::lock(&self.tasks);
        while tasks.try_join_next().is_some() {}
        tasks.spawn(task);
    }

    /// Stops every task the fleet runs (launches, claims, "Mark gone" after a silent
    /// answer) and waits until they have ended.
    pub async fn shutdown(&self) {
        let mut tasks = std::mem::take(&mut *crate::lock(&self.tasks));
        tasks.shutdown().await;
    }

    /// The build claims use, and `retire_before_ms`: on one host the router's own build;
    /// elsewhere the rollout record's `claim`.
    pub(crate) async fn claim_build(&self) -> Result<(BuildSpec, Option<i64>), StateError> {
        let own = BuildSpec {
            build: self.config.build.version.clone(),
            spec: self.config.spec.clone(),
        };
        let record = self.meta.rollout().await?;
        let retire = record.as_ref().and_then(|r| r.value.retire_before_ms);
        let claim = match record {
            Some(r) if !self.config.one_host => r.value.claim,
            _ => own,
        };
        Ok((claim, retire))
    }

    /// How many leases of one user may become warm in 10 minutes.
    fn claim_cap(&self) -> usize {
        let by_idle = 1200 / self.config.containers.idle_seconds.max(1);
        usize::try_from(by_idle).unwrap_or(usize::MAX).max(6)
    }

    /// Waits until the user's lease is `warm`, claiming a container if it is parked, for
    /// at most `wait`. The answer when time runs out is `upgrading` while the lease asks
    /// for an upgrade, else `unavailable`, both with `Retry-After: 2`.
    pub async fn acquire(
        self: &Arc<Self>,
        user: UserId,
        wait: Duration,
    ) -> Result<Versioned<Lease>, ApiError> {
        let deadline = Instant::now() + wait;
        let mut upgrading;
        loop {
            let read = self.leases.get(user).await.map_err(db_error)?;
            let lease = read.as_ref().map(|r| r.value.clone()).unwrap_or_default();
            let now = self.now();
            if let Some(e) = paused(&lease, now) {
                return Err(e);
            }
            upgrading = lease.park_requested() == Some(ParkReason::Upgrade);
            let step = match (lease.state, read) {
                (LeaseState::Warm, Some(read)) => return Ok(read),
                (LeaseState::Parked, read) => self.try_claim(user, read.as_ref()).await?,
                _ => Step::Wait,
            };
            let pause = if step == Step::Wait {
                POLL
            } else {
                Duration::ZERO
            };
            if Instant::now() + pause >= deadline {
                break;
            }
            tokio::time::sleep(pause).await;
        }
        Err(if upgrading {
            ApiError::new(
                Code::Upgrading,
                "Knoggin is being upgraded; try again shortly",
            )
            .with_retry_after(2)
        } else {
            ApiError::unavailable(2)
        })
    }

    /// Claim steps 1 and 2: picks a ready container of the claim build and writes the
    /// lease `claiming` and the container `claimed` in one write; steps 3 and 4 run as a
    /// task of the router's own.
    async fn try_claim(
        self: &Arc<Self>,
        user: UserId,
        read: Option<&Versioned<Lease>>,
    ) -> Result<Step, ApiError> {
        let now = self.now();
        let lease = read.map(|r| r.value.clone()).unwrap_or_default();
        if lease.claims(now) >= self.claim_cap() {
            metrics::counter!("knoggin_claims_total", "outcome" => "capped").increment(1);
            return Err(ApiError::unavailable(60).with_reason("crashed"));
        }
        let (build, retire_before) = self.claim_build().await.map_err(db_error)?;
        let records = self.containers.list().await.map_err(db_error)?;
        let candidate = records.iter().find(|c| {
            c.value.state == ContainerState::Ready
                && c.value.build == build.build
                && c.value.stop_reason().is_none()
                && retire_before.is_none_or(|t| c.value.started_ms >= t)
                && peer_of(c.value.address.as_deref(), c.value.cert_sha256.as_deref()).is_some()
        });
        let Some(found) = candidate else {
            let starting = records
                .iter()
                .any(|c| c.value.state == ContainerState::Starting && c.value.build == build.build);
            if starting {
                return Ok(Step::Wait);
            }
            if self.at_max(&records) {
                return Err(ApiError::unavailable(30));
            }
            // While the launcher's containers report another build than this router's,
            // no launch can bring a claimable container: one a minute, as the pool does.
            if self.launches_mismatch() {
                self.launch_slowly(build).await.map_err(db_error)?;
            } else {
                self.launch(build).await.map_err(db_error)?;
            }
            return Ok(Step::Wait);
        };
        let generation = lease.next_generation(now);
        let mut next = lease;
        next.set_state(LeaseState::Claiming, now);
        next.generation = generation;
        next.container_id = Some(found.value.id);
        next.handle.clone_from(&found.value.handle);
        next.address.clone_from(&found.value.address);
        next.cert_sha256.clone_from(&found.value.cert_sha256);
        next.build = Some(found.value.build.clone());
        next.claimed_by = Some(self.config.router_id.to_string());
        let mut container = found.value.clone();
        container.state = ContainerState::Claimed;
        container.user_id = Some(user);
        let mut writes = Leases::writes(user, &next, cond_of(read)).map_err(db_error)?;
        writes.push(Containers::write(&container, found.cond()).map_err(db_error)?);
        match self.db.write(writes).await {
            Ok(()) => {
                let fleet = self.clone();
                self.spawn(async move { fleet.finish_claim(user, container, generation).await });
                Ok(Step::Again)
            }
            // Another router got there first, or the lease changed: look again.
            Err(StateError::Conflict) => Ok(Step::Again),
            Err(e) => Err(db_error(e)),
        }
    }

    /// Whether `containers.max` is reached by the records that are not `gone`.
    pub(crate) fn at_max(&self, records: &[Versioned<Container>]) -> bool {
        let max = self.config.containers.max;
        max > 0
            && records
                .iter()
                .filter(|c| c.value.state != ContainerState::Gone)
                .count()
                >= max as usize
    }

    /// Claim steps 3 and 4.
    pub(crate) async fn finish_claim(
        self: Arc<Self>,
        user: UserId,
        container: Container,
        generation: u64,
    ) {
        let body = ClaimRequest {
            user_id: user,
            generation,
            settings: self.config.claim.clone(),
        };
        let accepted = match peer_of(
            container.address.as_deref(),
            container.cert_sha256.as_deref(),
        ) {
            Some(peer) => matches!(
                self.call(
                    container.id,
                    peer,
                    Method::POST,
                    api::paths::CLAIM,
                    Some(&body),
                    CLAIM_LIMIT,
                    false
                )
                .await,
                Ok((StatusCode::OK, _))
            ),
            None => false,
        };
        if !accepted {
            metrics::counter!("knoggin_claims_total", "outcome" => "failed").increment(1);
            tracing::warn!(kind = "claim_failed", container_id = %container.id, "a container refused or missed its claim");
            if let Err(e) = self
                .mark_gone(container.id, Some(StopReason::ClaimFailed), Caller::Request)
                .await
            {
                tracing::warn!(kind = "mark_gone", error = %e, "a failed claim was not tidied up");
            }
            return;
        }
        metrics::counter!("knoggin_claims_total", "outcome" => "ok").increment(1);
        if let Err(e) = self.set_warm(user, container.id, generation).await {
            tracing::warn!(kind = "claim", error = %e, "a claimed lease was not set warm");
        }
    }

    /// Claim step 4 (and reconcile step 5): the lease becomes `warm` and records the
    /// claim, if it is still `claiming` with this generation and this container.
    pub(crate) async fn set_warm(
        &self,
        user: UserId,
        container: Uuid,
        generation: u64,
    ) -> Result<bool, StateError> {
        for _ in 0..5 {
            let Some(read) = self.leases.get(user).await? else {
                return Ok(false);
            };
            let l = &read.value;
            if l.state != LeaseState::Claiming
                || l.generation != generation
                || l.container_id != Some(container)
            {
                return Ok(false);
            }
            let now = self.now();
            let mut next = l.clone();
            next.set_state(LeaseState::Warm, now);
            next.add_claim(now);
            match self.leases.put(user, &next, read.cond()).await {
                Ok(()) => return Ok(true),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable("the lease kept changing".into()))
    }

    /// Records a park request on the user's lease (it keeps one that is already there).
    pub async fn request_park(&self, user: UserId, reason: ParkReason) -> Result<(), StateError> {
        for _ in 0..5 {
            let Some(read) = self.leases.get(user).await? else {
                return Ok(());
            };
            if read.value.state == LeaseState::Parked {
                return Ok(());
            }
            let mut next = read.value.clone();
            if !next.request_park(reason, self.now()) {
                return Ok(());
            }
            match self.leases.put(user, &next, read.cond()).await {
                Ok(()) => return Ok(()),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable("the lease kept changing".into()))
    }

    /// "Mark gone" for a container. `reason` is written and the container stopped if it
    /// still runs; with `None` a running container is left alone. A `ready` container is
    /// first written `gone` (a racing claim loses), then stopped.
    pub async fn mark_gone(
        &self,
        id: Uuid,
        reason: Option<StopReason>,
        caller: Caller,
    ) -> Result<Marked, GoneError> {
        for _ in 0..5 {
            let Some(rec) = self.containers.get(id).await? else {
                self.peers.remove(id);
                return Ok(Marked::Gone);
            };
            match rec.value.state {
                ContainerState::Gone => {
                    self.peers.remove(id);
                    return Ok(Marked::Gone);
                }
                ContainerState::Ready => {
                    if let Some(r) = reason {
                        match self.retire_ready(&rec, r).await {
                            Err(GoneError::State(StateError::Conflict)) => continue,
                            other => return other.map(|()| Marked::Gone),
                        }
                    }
                }
                ContainerState::Starting | ContainerState::Claimed => {}
            }
            let mut info = self.find(&rec.value).await?;
            if info.as_ref().is_some_and(runs) {
                let Some(r) = reason else {
                    return Ok(Marked::Running);
                };
                let mut written = rec.value.clone();
                if written.set_stop_reason(r) {
                    match self.containers.put(&written, rec.cond()).await {
                        Ok(()) => {}
                        Err(StateError::Conflict) => continue,
                        Err(e) => return Err(e.into()),
                    }
                }
                let Some(handle) = info.as_ref().map(|i| i.handle.clone()) else {
                    return Ok(Marked::Running);
                };
                let why = written.stop_reason().map_or("", reason_str);
                self.launcher.stop(&handle, why).await?;
                info = self.find(&written).await?;
                if info.as_ref().is_some_and(runs) {
                    return Ok(Marked::Running);
                }
                return self.end(id, info.as_ref(), caller).await;
            }
            return self.end(id, info.as_ref(), caller).await;
        }
        Err(StateError::Unavailable("the container record kept changing".into()).into())
    }

    /// Steps 2 to 4 once the container no longer runs.
    async fn end(
        &self,
        id: Uuid,
        info: Option<&ContainerInfo>,
        caller: Caller,
    ) -> Result<Marked, GoneError> {
        let Some(rec) = self.containers.get(id).await? else {
            return Ok(Marked::Gone);
        };
        // On ECS a task the API does not know is unknown on the request path; only the
        // reconcile loop decides, once two lists in a row have missed it.
        if info.is_none()
            && !self.config.one_host
            && !matches!(caller, Caller::Reconcile { unseen: true })
        {
            return Ok(Marked::Unknown);
        }
        let end = match (rec.value.stop_reason(), info) {
            (Some(r), _) => End::of_reason(r),
            (None, Some(info)) => End::of_info(info),
            // On one host a clean exit is removed at once, so an unknown container parked.
            (None, None) if self.config.one_host => End::Parked,
            (None, None) => End::Failed,
        };
        self.finish(id, end).await?;
        Ok(Marked::Gone)
    }

    /// The `ready` order of "Mark gone": the write from `ready` to `gone` first, then the
    /// stop. A stop that fails is repeated by the reconcile loop (the record is `gone`).
    pub(crate) async fn retire_ready(
        &self,
        rec: &Versioned<Container>,
        reason: StopReason,
    ) -> Result<(), GoneError> {
        let mut c = rec.value.clone();
        c.set_stop_reason(reason);
        c.state = ContainerState::Gone;
        c.gone_ms = Some(self.now());
        self.containers.put(&c, rec.cond()).await?;
        self.peers.remove(c.id);
        let why = c.stop_reason().map_or("", reason_str);
        if let Some(h) = &c.handle
            && let Err(e) = self.launcher.stop(&LaunchHandle(h.clone()), why).await
        {
            tracing::warn!(kind = "stop", error = %e, "a retired pool container was not stopped yet");
        }
        Ok(())
    }

    /// The launcher's facts of a container: by its handle, or without one (the router
    /// died before storing it) by the ID tag in the launcher's list.
    async fn find(&self, c: &Container) -> Result<Option<ContainerInfo>, LaunchError> {
        match &c.handle {
            Some(h) => Ok(self
                .launcher
                .describe(&[LaunchHandle(h.clone())])
                .await?
                .into_iter()
                .find(|i| i.handle.0 == *h)),
            None => Ok(self
                .launcher
                .list()
                .await?
                .into_iter()
                .find(|i| i.container_id == Some(c.id))),
        }
    }

    /// Steps 3 and 4: in one write, the record `gone` and the lease that names it parked
    /// (with an abnormal end of a warm lease added).
    pub(crate) async fn finish(&self, id: Uuid, end: End) -> Result<(), StateError> {
        for _ in 0..5 {
            let Some(rec) = self.containers.get(id).await? else {
                return Ok(());
            };
            if rec.value.state == ContainerState::Gone {
                self.peers.remove(id);
                return Ok(());
            }
            let now = self.now();
            let mut c = rec.value.clone();
            c.state = ContainerState::Gone;
            c.gone_ms = Some(now);
            let mut writes = vec![Containers::write(&c, rec.cond())?];
            if let Some(user) = c.user_id
                && let Some(l) = self.leases.get(user).await?
                && l.value.container_id == Some(id)
                && l.value.state != LeaseState::Parked
            {
                let mut next = l.value.clone();
                if next.state == LeaseState::Warm
                    && let Some(kind) = end.abnormal()
                {
                    next.add_abnormal_end(now, kind);
                }
                next.set_state(LeaseState::Parked, now);
                writes.extend(Leases::writes(user, &next, l.cond())?);
            }
            match self.db.write(writes).await {
                Ok(()) => {
                    self.peers.remove(id);
                    metrics::counter!("knoggin_container_ends_total", "reason" => end.as_str())
                        .increment(1);
                    return Ok(());
                }
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable("the records kept changing".into()))
    }

    /// The broker's `parked`: steps 2 to 4 of "Mark gone" with the end `parked` (or the
    /// record's `stop_reason`), then a stop in case the container lingers.
    pub async fn finish_parked(&self, id: Uuid) -> Result<(), StateError> {
        let Some(rec) = self.containers.get(id).await? else {
            return Ok(());
        };
        let end = rec.value.stop_reason().map_or(End::Parked, End::of_reason);
        self.finish(id, end).await?;
        if let Some(h) = &rec.value.handle
            && self
                .launcher
                .stop(&LaunchHandle(h.clone()), "parked")
                .await
                .is_err()
        {
            tracing::debug!(
                kind = "stop",
                "a parked container's stop is left to the reconcile loop"
            );
        }
        Ok(())
    }

    /// One call to a container's internal route, with the router token, read in full (at
    /// most 1 MiB) within `limit`. `fresh` opens a new connection for it.
    #[expect(clippy::too_many_arguments, reason = "one call's whole description")]
    pub(crate) async fn call<T: Serialize>(
        &self,
        id: Uuid,
        peer: Peer,
        method: Method,
        path: &str,
        body: Option<&T>,
        limit: Duration,
        fresh: bool,
    ) -> Result<(StatusCode, Bytes), CallFailed> {
        self.peers.set(id, peer);
        let uri = format!("https://{id}:{}{path}", peer.address.port());
        let mut builder = Request::builder().method(method).uri(uri);
        let mut headers = HeaderMap::new();
        write_router_call(
            &mut headers,
            &self.config.secret.launch_secret(id).router_token(),
        );
        let body = match body {
            Some(b) => {
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                Body::from(serde_json::to_vec(b).map_err(|_| CallFailed::Link)?)
            }
            None => Body::empty(),
        };
        if let Some(h) = builder.headers_mut() {
            h.extend(headers);
        }
        let request = builder.body(body).map_err(|_| CallFailed::Link)?;
        let client = if fresh { &self.fresh } else { &self.client };
        let call = async {
            let response = client
                .request(request)
                .await
                .map_err(|_| CallFailed::Link)?;
            let status = response.status();
            let bytes = Limited::new(response.into_body(), MAX_ANSWER_BYTES)
                .collect()
                .await
                .map_err(|_| CallFailed::Link)?
                .to_bytes();
            Ok((status, bytes))
        };
        tokio::time::timeout(limit, call)
            .await
            .map_err(|_| CallFailed::Link)?
    }

    /// The container's `status`, on a new connection, within `limit`; `None` when it does
    /// not answer in time or answers something else.
    pub async fn status(
        &self,
        id: Uuid,
        peer: Peer,
        store: Option<StoreId>,
        limit: Duration,
    ) -> Option<StatusAnswer> {
        let path = match store {
            Some(s) => format!("{}?store={s}", api::paths::STATUS),
            None => api::paths::STATUS.to_owned(),
        };
        match self
            .call::<()>(id, peer, Method::GET, &path, None, limit, true)
            .await
        {
            Ok((StatusCode::OK, body)) => serde_json::from_slice(&body).ok(),
            _ => None,
        }
    }

    /// Asks a container to park; whether it accepted.
    pub async fn park(&self, id: Uuid, peer: Peer, reason: ParkReason) -> bool {
        matches!(
            self.call(
                id,
                peer,
                Method::POST,
                api::paths::PARK,
                Some(&ParkRequest { reason }),
                CLAIM_LIMIT,
                false
            )
            .await,
            Ok((StatusCode::ACCEPTED | StatusCode::OK, _))
        )
    }
}

/// Whether the launcher reports a container as still running (or about to).
pub(crate) fn runs(info: &ContainerInfo) -> bool {
    !matches!(info.state, crate::launcher::RunState::Ended)
}

/// The back-off for abnormal ends: while three lie within the last 10 minutes, the user's
/// requests answer `unavailable` with `Retry-After: 60`.
fn paused(lease: &Lease, now: i64) -> Option<ApiError> {
    let ends = lease.abnormal_ends(now);
    if ends.len() < 3 {
        return None;
    }
    let reason = match ends.last().map(|e| e.kind) {
        Some(EndKind::Memory) => "out_of_memory",
        _ => "crashed",
    };
    Some(ApiError::unavailable(60).with_reason(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_needs_three_ends_and_names_the_newest() {
        let mut l = Lease::default();
        l.add_abnormal_end(1_000, EndKind::Failed);
        l.add_abnormal_end(2_000, EndKind::Failed);
        assert!(paused(&l, 3_000).is_none());
        l.add_abnormal_end(3_000, EndKind::Memory);
        let e = paused(&l, 3_000).unwrap();
        assert_eq!(
            (e.code, e.reason, e.retry_after),
            (Code::Unavailable, Some("out_of_memory"), Some(60))
        );
        // It ends by itself once the oldest is 10 minutes old.
        assert!(paused(&l, 1_000 + 600_000).is_none());
    }
}
