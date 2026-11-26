// SPDX-License-Identifier: AGPL-3.0-only
//! Rollout (tab "Server", Rollout): the rollout record at a router's start, and the
//! leader's rollout steps on each pass. A user container never changes build in place:
//! it parks, and the next claim takes a container of the claim build. On one host the
//! claim build is the router's own and the record's `claim` and `desired` are not used.

use std::cmp::Ordering;
use std::sync::Arc;

use knoggin_state::record::{BuildSpec, ContainerState, ParkReason, Rollout, StopReason};
use knoggin_state::{Cond, Meta, StateError, Versioned};

use crate::launcher::BuildInfo;
use crate::lease::Fleet;
use crate::reconcile::Pass;

/// Step 9 of a router's start: writes the rollout record. None there: this router as
/// `claim` and `desired`. A newer build, or the same build with another spec, becomes
/// `desired`; an older one leaves the record alone unless `allow_downgrade` is set. Each
/// write is conditional; a router that loses reads again.
pub async fn at_start(
    meta: &Meta,
    own: &BuildSpec,
    allow_downgrade: bool,
) -> Result<(), StateError> {
    for _ in 0..5 {
        let read = meta.rollout().await?;
        let (next, cond) = match &read {
            None => (
                Rollout {
                    claim: own.clone(),
                    desired: own.clone(),
                    retire_before_ms: None,
                },
                Cond::Absent,
            ),
            Some(r) => {
                let order = BuildInfo::new(own.build.clone())
                    .compare(&BuildInfo::new(r.value.desired.build.clone()));
                let take = match order {
                    Some(Ordering::Greater) => true,
                    Some(Ordering::Equal) => own.spec != r.value.desired.spec,
                    Some(Ordering::Less) => allow_downgrade,
                    None => false,
                };
                if !take || r.value.desired == *own {
                    return Ok(());
                }
                let mut next = r.value.clone();
                next.desired = own.clone();
                (next, r.cond())
            }
        };
        match meta.put_rollout(&next, cond).await {
            Ok(()) => return Ok(()),
            Err(StateError::Conflict) => {}
            Err(e) => return Err(e),
        }
    }
    Err(StateError::Unavailable(
        "the rollout record kept changing".into(),
    ))
}

/// What the rollout steps found, for the pool step and the metric.
#[derive(Debug, Clone, Default)]
pub(crate) struct RolloutView {
    /// The build the pool keeps ready.
    pub(crate) claim: Option<BuildSpec>,
    /// Launch at most one container a minute: a stalled rollout, or a launcher whose
    /// containers report another build than the router's.
    pub(crate) slowly: bool,
}

impl Fleet {
    /// Step 10 of the reconcile loop.
    pub(crate) async fn rollout_steps(
        self: &Arc<Self>,
        pass: &Pass,
        launcher_up: bool,
    ) -> Result<RolloutView, StateError> {
        let own = BuildInfo::new(self.config.build.version.clone());
        let record = self.meta.rollout().await?;
        // Urgent upgrade: park in batches the leases whose containers started before
        // `retire_before_ms`.
        if let Some(t) = record.as_ref().and_then(|r| r.value.retire_before_ms) {
            let mut budget = batch(pass.active.len());
            for (user, lease) in &pass.active {
                if budget == 0 {
                    break;
                }
                if lease.value.park_requested().is_some() {
                    continue;
                }
                let started = lease
                    .value
                    .container_id
                    .and_then(|id| pass.record(id))
                    .map(|c| c.value.started_ms);
                if started.is_some_and(|s| s < t) {
                    match self.request_park(*user, ParkReason::Upgrade).await {
                        Ok(()) => budget -= 1,
                        Err(e) => crate::reconcile::step_failed("rollout", &e),
                    }
                }
            }
        }
        if self.config.one_host {
            // The launcher starts its own image: while it reports another build than the
            // router's, one of the two is still being upgraded.
            let other = self.launches_mismatch();
            metrics::gauge!("knoggin_rollout_stalled").set(f64::from(u8::from(other)));
            return Ok(RolloutView {
                claim: Some(BuildSpec {
                    build: own.version,
                    spec: self.config.spec.clone(),
                }),
                slowly: other,
            });
        }
        let Some(mut record) = record else {
            return Ok(RolloutView::default());
        };
        let desired = BuildInfo::new(record.value.desired.build.clone());
        let leads_rollout = own.compare(&desired).is_some_and(|o| o != Ordering::Less);
        if !leads_rollout || record.value.claim == record.value.desired {
            metrics::gauge!("knoggin_rollout_stalled").set(0.0);
            return Ok(RolloutView {
                claim: Some(record.value.claim),
                slowly: false,
            });
        }
        let of = |b: &BuildSpec, state: ContainerState| {
            pass.records.iter().any(|c| {
                c.value.build == b.build && c.value.spec == b.spec && c.value.state == state
            })
        };
        let desired_ready = of(&record.value.desired, ContainerState::Ready);
        let claim_alive = [
            ContainerState::Starting,
            ContainerState::Ready,
            ContainerState::Claimed,
        ]
        .into_iter()
        .any(|s| of(&record.value.claim, s));
        if desired_ready || !claim_alive {
            self.switch_claims(&mut record).await?;
            metrics::gauge!("knoggin_rollout_stalled").set(0.0);
            // The pool step stops the ready containers of the old build.
            return Ok(RolloutView {
                claim: Some(record.value.claim),
                slowly: false,
            });
        }
        // Not ready yet: try one container of the desired build a minute.
        let stalled = pass.records.iter().any(|c| {
            c.value.build == record.value.desired.build
                && c.value.state == ContainerState::Gone
                && c.value.stop_reason() == Some(StopReason::LaunchFailed)
        });
        metrics::gauge!("knoggin_rollout_stalled").set(f64::from(u8::from(stalled)));
        if launcher_up && !of(&record.value.desired, ContainerState::Starting) {
            self.launch_slowly(record.value.desired.clone()).await?;
        }
        Ok(RolloutView {
            claim: Some(record.value.claim),
            slowly: false,
        })
    }

    /// Sets `claim` to `desired`.
    async fn switch_claims(&self, record: &mut Versioned<Rollout>) -> Result<(), StateError> {
        let mut next = record.value.clone();
        next.claim = next.desired.clone();
        self.meta.put_rollout(&next, record.cond()).await?;
        tracing::info!(kind = "rollout", build = %next.claim.build, "claims switched to a new build");
        record.value = next;
        Ok(())
    }
}

/// 10% of `n` (at least one): how many leases one pass may ask to park.
pub(crate) fn batch(n: usize) -> usize {
    (n / 10).max(1)
}
