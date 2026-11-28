// SPDX-License-Identifier: AGPL-3.0-only
//! Launching and the pool: the record is written
//! `starting` before the launcher is called; the rest of a launch (the handle, waiting
//! for the address, `hello` with its proof, `ready`) runs as a task of the router's own,
//! bounded by 120 seconds. After a refused launch a router launches at most once a minute.

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{Request, StatusCode};
use http_body_util::Empty;
use knoggin_http::ctx::{PROTOCOL_VERSION, headers};
use knoggin_state::StateError;
use knoggin_state::record::{BuildSpec, Container, ContainerState, StopReason};
use tokio::time::Instant;
use uuid::Uuid;

use crate::launcher::{ContainerEnv, ContainerInfo, ContainerSpec, LaunchError, LaunchHandle};
use crate::lease::{Caller, Fleet, runs};
use crate::link::api::{self, HelloAnswer, shared_protocol};
use crate::link::{CertPin, Nonce, Peer, fetch_unpinned};

/// How long a container may take from launch to a checked `hello`.
pub const START_LIMIT: Duration = Duration::from_secs(120);

/// How often a starting container is described and greeted.
const EVERY: Duration = Duration::from_secs(1);

/// The limit of one `hello` call.
const HELLO_LIMIT: Duration = Duration::from_secs(5);

/// Launches after a refusal, or of a build the launcher does not yet run: at most one a
/// minute.
#[derive(Debug, Default)]
pub(crate) struct LaunchGate {
    /// When the last refused launch happened.
    refused_ms: Option<i64>,
    /// When the last held-back launch was let through (`launch_slowly`).
    slow_ms: Option<i64>,
    /// The last checked `hello` reported another build than the one launched.
    mismatch: bool,
}

impl Fleet {
    /// Starts one container of `build`: writes its record `starting`, then runs the
    /// launch as a task of the router's own. Returns `false` when a refused launch in the
    /// last minute holds launches back.
    pub async fn launch(self: &Arc<Self>, build: BuildSpec) -> Result<bool, StateError> {
        let now = self.now();
        {
            let gate = crate::lock(&self.gate);
            if gate.refused_ms.is_some_and(|t| now - t < 60_000) {
                return Ok(false);
            }
        }
        let id = Uuid::now_v7();
        let record = Container::starting(
            id,
            build.build.clone(),
            build.spec.clone(),
            now,
            self.retire_at(now),
        );
        self.containers
            .put(&record, knoggin_state::Cond::Absent)
            .await?;
        metrics::counter!("knoggin_launches_total", "outcome" => "started").increment(1);
        let fleet = self.clone();
        self.spawn(async move { fleet.run_launch(id, build.spec).await });
        Ok(true)
    }

    /// Like [`Self::launch`], but at most once a minute: a rollout that is stalled, or a
    /// launcher whose containers report another build than the router's.
    pub(crate) async fn launch_slowly(
        self: &Arc<Self>,
        build: BuildSpec,
    ) -> Result<bool, StateError> {
        let now = self.now();
        {
            let mut gate = crate::lock(&self.gate);
            if gate.slow_ms.is_some_and(|t| now - t < 60_000) {
                return Ok(false);
            }
            gate.slow_ms = Some(now);
        }
        self.launch(build).await
    }

    /// Whether the last checked `hello` reported another build than the one launched.
    pub(crate) fn launches_mismatch(&self) -> bool {
        crate::lock(&self.gate).mismatch
    }

    /// The start time plus `containers.max_lifetime_hours`, less a random 0 to 4 hours.
    fn retire_at(&self, now: i64) -> i64 {
        let life = i64::try_from(self.config.containers.max_lifetime_hours)
            .unwrap_or(i64::MAX / 3_600_000)
            .saturating_mul(3_600_000);
        let mut bytes = [0u8; 8];
        let spread = if aws_lc_rs::rand::fill(&mut bytes).is_ok() {
            (u64::from_le_bytes(bytes) % (4 * 3_600_000)) as i64
        } else {
            0
        };
        now.saturating_add(life).saturating_sub(spread)
    }

    /// The rest of a launch after the `starting` write: the handle, the address, `hello`, `ready`.
    async fn run_launch(self: Arc<Self>, id: Uuid, spec: String) {
        let deadline = Instant::now() + START_LIMIT;
        match tokio::time::timeout_at(deadline, self.start(id, spec)).await {
            Ok(Ok(())) => {}
            Ok(Err(Failed::Refused)) => {}
            Ok(Err(Failed::Bad)) | Err(_) => {
                metrics::counter!("knoggin_launches_total", "outcome" => "failed").increment(1);
                if let Err(e) = self
                    .mark_gone(id, Some(StopReason::LaunchFailed), Caller::Request)
                    .await
                {
                    tracing::warn!(kind = "launch_failed", error = %e, "a failed launch is left to the reconcile loop");
                }
            }
        }
    }

    async fn start(&self, id: Uuid, spec: String) -> Result<(), Failed> {
        let env = ContainerEnv {
            container_id: id,
            launch_secret: self.config.secret.launch_secret(id),
            router_url: self.config.router_url.clone(),
            router_cert_sha256: self.config.router_pin,
            log_level: self.config.log_level,
        };
        let handle = match self
            .launcher
            .launch(&ContainerSpec {
                container_id: id,
                spec,
                env,
            })
            .await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(kind = "launch_refused", error = %e, "the launcher refused a launch");
                metrics::counter!("knoggin_launches_total", "outcome" => "refused").increment(1);
                crate::lock(&self.gate).refused_ms = Some(self.now());
                let now = self.now();
                self.update(id, |c| {
                    c.state = ContainerState::Gone;
                    c.gone_ms = Some(now);
                })
                .await
                .map_err(|_| Failed::Bad)?;
                return Err(Failed::Refused);
            }
        };
        self.update(id, |c| c.handle = Some(handle.0.clone()))
            .await
            .map_err(|_| Failed::Bad)?;
        // Wait for the launcher to report it running with an address.
        let info = loop {
            match self.launcher.describe(std::slice::from_ref(&handle)).await {
                Ok(list) => match list.into_iter().find(|i| i.handle == handle) {
                    Some(i) if i.address.is_some() && runs(&i) => break i,
                    Some(i) if !runs(&i) => return Err(Failed::Bad),
                    _ => {}
                },
                Err(LaunchError::Unavailable) => {}
                Err(_) => return Err(Failed::Bad),
            }
            tokio::time::sleep(EVERY).await;
        };
        let Some(address) = info.address else {
            return Err(Failed::Bad);
        };
        self.record_address(id, address, &info)
            .await
            .map_err(|_| Failed::Bad)?;
        // `hello` every second until it answers.
        let (answer, pin) = loop {
            match self.hello(id, address).await {
                Hello::Answer(a, pin) => break (*a, pin),
                Hello::NotYet => tokio::time::sleep(EVERY).await,
                Hello::Bad => return Err(Failed::Bad),
            }
        };
        let expected = self
            .containers
            .get(id)
            .await
            .ok()
            .flatten()
            .map(|c| c.value.build);
        crate::lock(&self.gate).mismatch = expected.is_some_and(|b| b != answer.build);
        if answer.test_mode != self.config.test_mode || shared_protocol(&answer.protocols).is_none()
        {
            tracing::warn!(kind = "launch_failed", container_id = %id, "a container's hello names another test mode or no shared protocol");
            return Err(Failed::Bad);
        }
        self.update(id, |c| {
            c.cert_sha256 = Some(pin.to_hex());
            c.build = answer.build.clone();
            c.state = ContainerState::Ready;
        })
        .await
        .map_err(|_| Failed::Bad)?;
        self.peers.set(id, Peer { address, pin });
        metrics::counter!("knoggin_launches_total", "outcome" => "ready").increment(1);
        Ok(())
    }

    /// Records a newly launched container's address. In the same step the router clears
    /// its counts for that IP, and runs "Mark gone" for every other live record naming the
    /// same address (IP and port).
    async fn record_address(
        &self,
        id: Uuid,
        address: SocketAddrV4,
        info: &ContainerInfo,
    ) -> Result<(), StateError> {
        let text = address.to_string();
        self.update(id, |c| {
            c.address = Some(text.clone());
            c.host.clone_from(&info.host);
        })
        .await?;
        self.limits.clear((*address.ip()).into());
        for other in self.containers.list().await? {
            if other.value.id != id
                && other.value.state != ContainerState::Gone
                && other.value.address.as_deref() == Some(text.as_str())
            {
                tracing::warn!(kind = "address_reused", container_id = %other.value.id, "an older container named a new container's address");
                if let Err(e) = self.mark_gone(other.value.id, None, Caller::Request).await {
                    tracing::warn!(kind = "mark_gone", error = %e, "an older container at a reused address is left to the reconcile loop");
                }
            }
        }
        Ok(())
    }

    /// Reads, changes and writes back a `starting` container's record.
    async fn update(&self, id: Uuid, change: impl Fn(&mut Container)) -> Result<(), StateError> {
        for _ in 0..5 {
            let Some(rec) = self.containers.get(id).await? else {
                return Err(StateError::NotFound);
            };
            if rec.value.state != ContainerState::Starting {
                return Err(StateError::Conflict);
            }
            let mut c = rec.value.clone();
            change(&mut c);
            match self.containers.put(&c, rec.cond()).await {
                Ok(()) => return Ok(()),
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(StateError::Unavailable(
            "the container record kept changing".into(),
        ))
    }

    /// One `hello`: a fresh connection, the proof checked against that connection's
    /// certificate with the launch secret the router derives itself.
    async fn hello(&self, id: Uuid, address: SocketAddrV4) -> Hello {
        let Ok(nonce) = Nonce::random() else {
            return Hello::NotYet;
        };
        let Ok(request) = Request::get(format!("{}?nonce={}", api::paths::HELLO, nonce.to_hex()))
            .header(http::header::HOST, id.to_string())
            .header(headers::PROTOCOL, PROTOCOL_VERSION)
            .body(Empty::<Bytes>::new())
        else {
            return Hello::Bad;
        };
        let reply = match tokio::time::timeout(
            HELLO_LIMIT,
            fetch_unpinned(SocketAddr::V4(address), &id.to_string(), request),
        )
        .await
        {
            Ok(Ok(r)) => r,
            _ => return Hello::NotYet,
        };
        if reply.status == StatusCode::SERVICE_UNAVAILABLE {
            return Hello::NotYet;
        }
        if reply.status != StatusCode::OK {
            return Hello::Bad;
        }
        let Ok(answer) = serde_json::from_slice::<HelloAnswer>(&reply.body) else {
            return Hello::Bad;
        };
        let launch = self.config.secret.launch_secret(id);
        if answer.container_id != id || !launch.check_proof(&nonce, &reply.cert_der, &answer.proof)
        {
            tracing::warn!(kind = "hello_refused", container_id = %id, "a hello's proof did not match");
            return Hello::Bad;
        }
        Hello::Answer(Box::new(answer), CertPin::of(&reply.cert_der))
    }

    /// Stops a running container that no record explains (the reconcile loop's strays).
    pub(crate) async fn stop_stray(&self, handle: &LaunchHandle) {
        if let Err(e) = self.launcher.stop(handle, "stray").await {
            tracing::warn!(kind = "stop", error = %e, "a stray container was not stopped");
        }
    }
}

enum Failed {
    /// The launcher refused; the record is already `gone`.
    Refused,
    /// Anything else: "Mark gone" with `launch_failed`.
    Bad,
}

enum Hello {
    Answer(Box<HelloAnswer>, CertPin),
    NotYet,
    Bad,
}
