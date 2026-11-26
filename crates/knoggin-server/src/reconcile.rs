// SPDX-License-Identifier: AGPL-3.0-only
//! The reconcile loop (tab "Server", The reconcile loop): one router at a time leads,
//! through the record `leader/reconcile`, taken when absent or expired or when this
//! router's build is newer than the holder's, and renewed every 10 seconds. Every 30
//! seconds the leader runs one pass of thirteen steps, each safe to repeat. When the
//! launcher cannot be reached, the steps that need it are skipped.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream;
use knoggin_core::UserId;
use knoggin_state::record::{
    Container, ContainerState, Leader, Lease, LeaseState, ParkReason, StopReason,
};
use knoggin_state::{Cond, StateError, Versioned};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use crate::launcher::{BuildInfo, ContainerInfo};
use crate::lease::{Caller, Fleet, peer_of, reason_str, runs};
use crate::link::api::{ContainerPhase, Counters};
use crate::rollout::batch;
use crate::storage::names;

/// How often a router tries to take or renew leadership.
pub const RENEW_EVERY: Duration = Duration::from_secs(10);
/// How long leadership lasts without renewal.
pub const LEADER_MS: i64 = 30_000;
/// How often the leader runs a pass.
pub const PASS_EVERY: Duration = Duration::from_secs(30);

const START_MS: i64 = 120_000;
const CLAIM_MS: i64 = 60_000;
const PARK_REQUEST_MS: i64 = 2 * 3_600_000;
const GONE_KEEP_MS: i64 = 3_600_000;
const STATUS_LIMIT: Duration = Duration::from_secs(5);

/// What one pass read at its start.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    /// What the launcher runs; `None` when it cannot be reached.
    pub(crate) listed: Option<Vec<ContainerInfo>>,
    /// Every container record.
    pub(crate) records: Vec<Versioned<Container>>,
    /// Every lease that is not parked.
    pub(crate) active: Vec<(UserId, Versioned<Lease>)>,
}

impl Pass {
    pub(crate) fn record(&self, id: Uuid) -> Option<&Versioned<Container>> {
        self.records.iter().find(|c| c.value.id == id)
    }

    pub(crate) fn by_handle(&self, handle: &str) -> Option<&Versioned<Container>> {
        self.records
            .iter()
            .find(|c| c.value.handle.as_deref() == Some(handle))
    }

    /// The launcher's facts of a record: by its handle, or without one by its ID tag.
    fn info(&self, c: &Container) -> Option<&ContainerInfo> {
        let listed = self.listed.as_ref()?;
        match &c.handle {
            Some(h) => listed.iter().find(|i| i.handle.0 == *h),
            None => listed.iter().find(|i| i.container_id == Some(c.id)),
        }
    }
}

/// What the leader keeps in memory between passes. A new leader starts afresh.
#[derive(Debug, Default)]
pub struct Reconciler {
    /// Passes in a row whose list did not show a record's container.
    unseen: HashMap<Uuid, u32>,
    /// The counters step of the last pass, if it still runs.
    counters: Option<JoinHandle<()>>,
}

impl Drop for Reconciler {
    fn drop(&mut self) {
        if let Some(h) = self.counters.take() {
            h.abort();
        }
    }
}

/// Runs the loop until `stop` resolves.
pub async fn run(fleet: Arc<Fleet>, stop: impl Future<Output = ()>) {
    let mut stop = std::pin::pin!(stop);
    let mut state = Reconciler::default();
    let mut last_pass: Option<Instant> = None;
    let mut tick = tokio::time::interval(RENEW_EVERY);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = &mut stop => return,
        }
        match fleet.take_leadership().await {
            Ok(true) => {}
            Ok(false) => {
                state = Reconciler::default();
                last_pass = None;
                continue;
            }
            Err(e) => {
                tracing::warn!(kind = "leader", error = %e, "leadership could not be checked");
                continue;
            }
        }
        if last_pass.is_none_or(|t| t.elapsed() >= PASS_EVERY) {
            last_pass = Some(Instant::now());
            let started = std::time::Instant::now();
            if let Err(e) = fleet.pass(&mut state).await {
                tracing::warn!(kind = "reconcile", error = %e, "a reconcile pass stopped early");
            }
            metrics::histogram!("knoggin_reconcile_seconds")
                .record(started.elapsed().as_secs_f64());
        }
    }
}

impl Fleet {
    /// Takes or renews the reconcile lease; whether this router leads now.
    pub async fn take_leadership(&self) -> Result<bool, StateError> {
        let me = Leader {
            router_id: self.config.router_id.to_string(),
            build: self.config.build.version.clone(),
            until_ms: self.now() + LEADER_MS,
        };
        let cond = match self.meta.leader().await? {
            None => Cond::Absent,
            Some(l) if l.value.router_id == me.router_id => l.cond(),
            Some(l)
                if self
                    .config
                    .build
                    .is_newer_than(&BuildInfo::new(l.value.build.clone())) =>
            {
                l.cond()
            }
            Some(_) => return Ok(false),
        };
        match self.meta.put_leader(&me, cond).await {
            Ok(()) => Ok(true),
            Err(StateError::Conflict) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Reads what a pass works on.
    async fn read_pass(&self) -> Result<Pass, StateError> {
        let listed = match self.launcher.list().await {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(kind = "launcher", error = %e, "the launcher cannot be reached; its steps are skipped");
                None
            }
        };
        let records = self.containers.list().await?;
        let mut active = Vec::new();
        let mut after = None;
        loop {
            let page = self.leases.list_active(100, after).await?;
            for &user in &page {
                if let Some(l) = self.leases.get(user).await? {
                    active.push((user, l));
                }
            }
            match page.last() {
                Some(last) if page.len() == 100 => after = Some(*last),
                _ => break,
            }
        }
        Ok(Pass {
            listed,
            records,
            active,
        })
    }

    /// One pass of the loop, in order.
    pub async fn pass(self: &Arc<Self>, state: &mut Reconciler) -> Result<(), StateError> {
        // 1. List.
        let pass = self.read_pass().await?;
        let up = pass.listed.is_some();
        let now = self.now();
        for state_name in [
            ContainerState::Starting,
            ContainerState::Ready,
            ContainerState::Claimed,
        ] {
            let n = pass
                .records
                .iter()
                .filter(|c| c.value.state == state_name)
                .count();
            metrics::gauge!("knoggin_containers", "state" => state_label(state_name)).set(n as f64);
        }
        if up {
            self.gone_containers(&pass, state).await;
            self.strays(&pass).await;
            // 4. Stuck starts.
            for c in &pass.records {
                if c.value.state == ContainerState::Starting && now - c.value.started_ms > START_MS
                {
                    self.gone(c.value.id, Some(StopReason::LaunchFailed)).await;
                }
            }
            self.stuck_claims(&pass).await;
        }
        self.leases_without_container(&pass).await;
        self.park_requests(&pass, up).await;
        if up {
            self.lifetime(&pass).await?;
            self.draining(&pass).await?;
        }
        let view = self.rollout_steps(&pass, up).await?;
        if up && let Some(claim) = view.claim {
            self.pool(&pass, &claim, view.slowly).await?;
        }
        // 12. Counters, beside the others.
        if state.counters.as_ref().is_none_or(JoinHandle::is_finished) {
            let fleet = self.clone();
            let claimed: Vec<Container> = pass
                .records
                .iter()
                .filter(|c| c.value.state == ContainerState::Claimed)
                .map(|c| c.value.clone())
                .collect();
            state.counters = Some(tokio::spawn(async move { fleet.counters(claimed).await }));
        }
        // 13. Cleanup.
        for c in &pass.records {
            if c.value.state == ContainerState::Gone
                && c.value.gone_ms.is_some_and(|t| now - t > GONE_KEEP_MS)
                && let Err(e) = self.containers.delete(c.value.id, c.cond()).await
                && !matches!(e, StateError::Conflict)
            {
                tracing::warn!(kind = "cleanup", error = %e, "a gone record was not deleted");
            }
        }
        Ok(())
    }

    /// "Mark gone" from the loop, which logs what it could not finish.
    async fn gone(&self, id: Uuid, reason: Option<StopReason>) {
        self.gone_as(id, reason, Caller::Reconcile { unseen: false })
            .await;
    }

    async fn gone_as(&self, id: Uuid, reason: Option<StopReason>, caller: Caller) {
        if let Err(e) = self.mark_gone(id, reason, caller).await {
            tracing::warn!(kind = "mark_gone", container_id = %id, error = %e, "mark gone did not finish; the next pass tries again");
        }
    }

    /// Step 2: records `ready` or `claimed` whose container does not run.
    async fn gone_containers(&self, pass: &Pass, state: &mut Reconciler) {
        let mut seen = HashSet::new();
        for c in &pass.records {
            if !matches!(
                c.value.state,
                ContainerState::Ready | ContainerState::Claimed
            ) {
                continue;
            }
            seen.insert(c.value.id);
            match pass.info(&c.value) {
                Some(i) if runs(i) => {
                    state.unseen.remove(&c.value.id);
                }
                Some(_) => self.gone(c.value.id, None).await,
                None => {
                    let n = state.unseen.entry(c.value.id).or_insert(0);
                    *n += 1;
                    let unseen = *n >= 2;
                    self.gone_as(c.value.id, None, Caller::Reconcile { unseen })
                        .await;
                }
            }
        }
        state.unseen.retain(|id, _| seen.contains(id));
    }

    /// Step 3: strays, claimed containers that no lease names, and stops asked for again.
    async fn strays(&self, pass: &Pass) {
        let Some(listed) = &pass.listed else {
            return;
        };
        for i in listed.iter().filter(|i| runs(i)) {
            match pass.by_handle(&i.handle.0) {
                None => {
                    let launching =
                        i.container_id
                            .and_then(|id| pass.record(id))
                            .is_some_and(|c| {
                                c.value.state == ContainerState::Starting
                                    && c.value.handle.is_none()
                            });
                    if !launching {
                        self.stop_stray(&i.handle).await;
                    }
                }
                Some(c) if i.container_id.is_some_and(|t| t != c.value.id) => {
                    tracing::error!(kind = "stray", container_id = %c.value.id, "a running container's tag names another record than its handle's; left alone");
                }
                Some(c) if c.value.state == ContainerState::Gone => {
                    self.stop_stray(&i.handle).await
                }
                Some(c) => {
                    if let Some(r) = c.value.stop_reason()
                        && let Err(e) = self.launcher.stop(&i.handle, reason_str(r)).await
                    {
                        tracing::warn!(kind = "stop", error = %e, "a stop asked for again failed");
                    }
                }
            }
        }
        for c in &pass.records {
            if c.value.state != ContainerState::Claimed {
                continue;
            }
            let Some(user) = c.value.user_id else {
                self.gone(c.value.id, Some(StopReason::Stray)).await;
                continue;
            };
            let names_it = pass
                .active
                .iter()
                .find(|(u, _)| *u == user)
                .is_some_and(|(_, l)| l.value.container_id == Some(c.value.id));
            if !names_it {
                // The pass's lease list may be older than the record: read again.
                let current = self.leases.get(user).await.ok().flatten();
                if current.is_some_and(|l| {
                    l.value.container_id == Some(c.value.id) && l.value.state != LeaseState::Parked
                }) {
                    continue;
                }
                self.gone(c.value.id, Some(StopReason::Stray)).await;
            }
        }
    }

    /// Step 5: leases `claiming` for more than 60 seconds.
    async fn stuck_claims(&self, pass: &Pass) {
        let now = self.now();
        for (user, lease) in &pass.active {
            let l = &lease.value;
            if l.state != LeaseState::Claiming || now - l.state_since_ms <= CLAIM_MS {
                continue;
            }
            let Some(id) = l.container_id else {
                continue;
            };
            let record = pass.record(id);
            let answer = match peer_of(l.address.as_deref(), l.cert_sha256.as_deref()) {
                Some(peer) => self.status(id, peer, None, STATUS_LIMIT).await,
                None => None,
            };
            let claimed = answer.is_some_and(|a| {
                a.state == ContainerPhase::Claimed
                    && a.user_id == Some(*user)
                    && a.generation == Some(l.generation)
            });
            if claimed && record.is_some_and(|c| c.value.stop_reason().is_none()) {
                if let Err(e) = self.set_warm(*user, id, l.generation).await {
                    tracing::warn!(kind = "stuck_claim", error = %e, "a stuck claim was not set warm");
                }
            } else {
                self.gone(id, Some(StopReason::ClaimFailed)).await;
            }
        }
    }

    /// Step 6: leases whose container record is `gone` or missing.
    async fn leases_without_container(&self, pass: &Pass) {
        for (user, lease) in &pass.active {
            let missing = lease.value.container_id.is_none_or(|id| {
                pass.record(id)
                    .is_none_or(|c| c.value.state == ContainerState::Gone)
            });
            if !missing {
                continue;
            }
            let mut next = lease.value.clone();
            next.set_state(LeaseState::Parked, self.now());
            match self.leases.put(*user, &next, lease.cond()).await {
                Ok(()) | Err(StateError::Conflict) => {}
                Err(e) => {
                    tracing::warn!(kind = "lease", error = %e, "a lease without a container stays")
                }
            }
        }
    }

    /// Step 7: park requests, and the 2-hour limit after the router's own bucket check.
    async fn park_requests(&self, pass: &Pass, up: bool) {
        let now = self.now();
        for (user, lease) in &pass.active {
            let l = &lease.value;
            let Some(reason) = l.park_requested() else {
                continue;
            };
            let Some(id) = l.container_id else {
                continue;
            };
            if l.park_requested_ms()
                .is_some_and(|t| now - t >= PARK_REQUEST_MS)
            {
                if up && self.bucket_answers(*user).await {
                    tracing::warn!(kind = "park_timeout", container_id = %id, "a container that did not park in 2 hours is stopped");
                    self.gone(id, Some(StopReason::ParkTimeout)).await;
                }
                continue;
            }
            if let Some(peer) = peer_of(l.address.as_deref(), l.cert_sha256.as_deref())
                && !self.park(id, peer, reason).await
            {
                tracing::debug!(kind = "park", container_id = %id, "a park call was not accepted");
            }
        }
    }

    /// Whether the router itself reaches the bucket: a listing of one of the user's
    /// stores' snapshot folder within 5 seconds.
    async fn bucket_answers(&self, user: UserId) -> bool {
        let Ok(stores) = self.stores.list(user).await else {
            return false;
        };
        let Some(store) = stores.first() else {
            return true;
        };
        let prefix = names::snapshot_prefix(store.value.id);
        matches!(
            tokio::time::timeout(STATUS_LIMIT, self.storage.list(&prefix)).await,
            Ok(Ok(_))
        )
    }

    /// Step 8: lifetime and `retire_before_ms`.
    async fn lifetime(&self, pass: &Pass) -> Result<(), StateError> {
        let now = self.now();
        let retire_before = self
            .meta
            .rollout()
            .await?
            .and_then(|r| r.value.retire_before_ms);
        for c in &pass.records {
            let v = &c.value;
            match v.state {
                ContainerState::Claimed if now > v.retire_at_ms => {
                    if let Some(user) = v.user_id {
                        self.request_park(user, ParkReason::Lifetime).await?;
                    }
                }
                ContainerState::Ready if now > v.retire_at_ms => {
                    self.retire(c, StopReason::Lifetime).await;
                }
                ContainerState::Ready if retire_before.is_some_and(|t| v.started_ms < t) => {
                    self.retire(c, StopReason::Rollout).await;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn retire(&self, c: &Versioned<Container>, reason: StopReason) {
        if let Err(e) = self.retire_ready(c, reason).await {
            tracing::debug!(kind = "retire", error = %e, "a pool container was claimed or changed first");
        }
    }

    /// Step 9: containers on draining hosts.
    async fn draining(&self, pass: &Pass) -> Result<(), StateError> {
        let Some(listed) = &pass.listed else {
            return Ok(());
        };
        let hosts: HashSet<&str> = listed
            .iter()
            .filter(|i| i.draining)
            .filter_map(|i| i.host.as_deref())
            .collect();
        metrics::gauge!("knoggin_hosts_draining").set(hosts.len() as f64);
        let mut budget = batch(pass.active.len());
        for c in &pass.records {
            let draining = pass.info(&c.value).is_some_and(|i| i.draining);
            if !draining {
                continue;
            }
            match c.value.state {
                ContainerState::Ready => self.retire(c, StopReason::Drain).await,
                ContainerState::Claimed if budget > 0 => {
                    if let Some(user) = c.value.user_id {
                        let asked = pass
                            .active
                            .iter()
                            .any(|(u, l)| *u == user && l.value.park_requested().is_some());
                        if !asked {
                            self.request_park(user, ParkReason::Drain).await?;
                            budget -= 1;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Step 11: launches up to the target (at most 10 per pass, never past
    /// `containers.max`), and retires ready containers of other builds and beyond it.
    async fn pool(
        self: &Arc<Self>,
        pass: &Pass,
        claim: &knoggin_state::record::BuildSpec,
        slowly: bool,
    ) -> Result<(), StateError> {
        let of_claim = |c: &&Versioned<Container>| c.value.build == claim.build;
        let mut ready: Vec<&Versioned<Container>> = pass
            .records
            .iter()
            .filter(|c| c.value.state == ContainerState::Ready && c.value.stop_reason().is_none())
            .collect();
        for c in ready.iter().filter(|c| !of_claim(c)) {
            self.retire(c, StopReason::Rollout).await;
        }
        ready.retain(of_claim);
        let starting = pass
            .records
            .iter()
            .filter(|c| c.value.state == ContainerState::Starting && c.value.build == claim.build)
            .count();
        let target = self.config.containers.pool_target as usize;
        let have = ready.len() + starting;
        if have < target {
            let max = self.config.containers.max as usize;
            let live = pass
                .records
                .iter()
                .filter(|c| c.value.state != ContainerState::Gone)
                .count();
            let room = if max == 0 {
                usize::MAX
            } else {
                max.saturating_sub(live)
            };
            let n = (target - have).min(10).min(room);
            for _ in 0..n {
                let launched = if slowly {
                    self.launch_slowly(claim.clone()).await?
                } else {
                    self.launch(claim.clone()).await?
                };
                if !launched {
                    break;
                }
            }
        } else if ready.len() > target {
            ready.sort_by_key(|c| std::cmp::Reverse(c.value.started_ms));
            let surplus = ready.len() - target;
            for c in ready.into_iter().take(surplus) {
                self.retire(c, StopReason::Surplus).await;
            }
        }
        Ok(())
    }

    /// Step 12: the sums of the claimed containers' counters, 16 at a time.
    async fn counters(self: Arc<Self>, claimed: Vec<Container>) {
        let answers: Vec<_> = stream::iter(claimed)
            .map(|c| {
                let fleet = self.clone();
                async move {
                    let peer = peer_of(c.address.as_deref(), c.cert_sha256.as_deref())?;
                    fleet.status(c.id, peer, None, STATUS_LIMIT).await
                }
            })
            .buffer_unordered(16)
            .collect()
            .await;
        let mut sum = Counters::default();
        let (mut open, mut queue, mut failures) = (0u64, 0u64, 0u64);
        for a in answers.into_iter().flatten() {
            sum = sum.saturating_add(a.counters);
            open = open.saturating_add(a.stores.len() as u64);
            queue = a
                .stores
                .iter()
                .fold(queue, |q, s| q.saturating_add(s.pending_embeddings));
            failures = failures.saturating_add(a.snapshot_failures);
        }
        metrics::counter!("knoggin_container_requests_total").absolute(sum.requests);
        metrics::counter!("knoggin_container_embed_count_total").absolute(sum.embed_count);
        metrics::counter!("knoggin_container_embed_millis_total").absolute(sum.embed_millis);
        metrics::counter!("knoggin_container_snapshot_count_total").absolute(sum.snapshot_count);
        metrics::counter!("knoggin_container_snapshot_millis_total").absolute(sum.snapshot_millis);
        metrics::counter!("knoggin_container_snapshot_bytes_total").absolute(sum.snapshot_bytes);
        metrics::counter!("knoggin_container_snapshot_failures_total").absolute(failures);
        metrics::gauge!("knoggin_container_open_stores").set(open as f64);
        metrics::gauge!("knoggin_container_embed_queue_depth").set(queue as f64);
    }
}

fn state_label(s: ContainerState) -> &'static str {
    match s {
        ContainerState::Starting => "starting",
        ContainerState::Ready => "ready",
        ContainerState::Claimed => "claimed",
        ContainerState::Gone => "gone",
    }
}
