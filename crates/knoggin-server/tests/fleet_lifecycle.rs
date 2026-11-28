// SPDX-License-Identifier: AGPL-3.0-only
//! Fleet tests of a store request's life: claim
//! (with races), park under each trigger, crashes, the reconcile loop, and rollout between
//! two builds side by side. Routers run through `router::run_router`; containers run the
//! real container role in the fake launcher.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod fleet_support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fleet_support::{Deployment, eventually, json, local_user, peer, save, summaries};
use futures_util::StreamExt;
use knoggin_core::UserId;
use knoggin_server::lease::{Caller, Marked};
use knoggin_server::reconcile::Reconciler;
use knoggin_server::router::run_router;
use knoggin_server::testing::{StubControl, serve_runner, stub_runner};
use knoggin_state::Meta;
use knoggin_state::record::{BuildSpec, ContainerState, LeaseState, ParkReason, Slot, StopReason};
use uuid::Uuid;

const LIMIT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn a_request_claims_a_container_and_the_store_survives_a_park() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (status, _) = save(&r, None, "first").await;
    assert_eq!(status, 201);
    let (user, _) = local_user(&d).await;
    let lease = d.lease(user).await;
    assert_eq!(lease.state, LeaseState::Warm);
    let first = lease.container_id.unwrap();
    // Router-asked park: the container saves through the broker and reports `parked`.
    let fleet = d
        .fleet("9.0.0", true, &format!("https://{}", r.internal))
        .await;
    assert!(
        fleet
            .park(
                first,
                peer(&d.containers().get(first).await.unwrap().unwrap().value),
                ParkReason::Restore
            )
            .await
    );
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    let c = d.containers().get(first).await.unwrap().unwrap().value;
    assert_eq!(c.state, ContainerState::Gone);
    assert!(
        eventually(LIMIT, || async { d.launcher.running() == 0 }).await,
        "the parked container ended"
    );
    // The next request claims a new container, which loads the snapshot.
    let (status, items) = summaries(&r, None).await;
    assert_eq!((status, items), (200, vec!["first".to_owned()]));
    assert_ne!(d.lease(user).await.container_id, Some(first));
}

#[tokio::test]
async fn two_routers_race_to_claim_one_user_and_both_requests_are_served() {
    let d = Deployment::new(false).await;
    let a = d.router("0.1.0").await;
    let b = d.router("0.1.0").await;
    let (x, y) = tokio::join!(summaries(&a, None), summaries(&b, None));
    assert_eq!((x.0, y.0), (200, 200));
    let (user, _) = local_user(&d).await;
    assert_eq!(d.lease(user).await.claims(d.clock.as_ref()()), 1);
    assert_eq!(d.count(ContainerState::Claimed).await, 1);
}

#[tokio::test]
async fn an_idle_container_parks_by_itself() {
    let d = Deployment::new(false).await;
    let r = d
        .router_with("0.1.0", |t| t.settings.containers.idle_seconds = 1)
        .await
        .unwrap();
    assert_eq!(save(&r, None, "idle").await.0, 201);
    let (user, _) = local_user(&d).await;
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    assert_eq!(summaries(&r, None).await.1, vec!["idle".to_owned()]);
}

#[tokio::test]
async fn a_crash_loses_what_no_snapshot_holds_and_the_store_still_loads() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&r, None, "saved").await.0, 201);
    let fleet = d
        .fleet("9.0.0", true, &format!("https://{}", r.internal))
        .await;
    let first = d.lease(user).await.container_id.unwrap();
    let c = d.containers().get(first).await.unwrap().unwrap().value;
    assert!(fleet.park(first, peer(&c), ParkReason::Lifetime).await);
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    // After the snapshot: a new container, a write, then a crash before any snapshot.
    assert_eq!(save(&r, None, "lost").await.0, 201);
    let second = d.lease(user).await.container_id.unwrap();
    d.launcher
        .kill(&d.launcher.handle_of(second).unwrap(), false);
    let (status, items) = summaries(&r, None).await;
    assert_eq!((status, items), (200, vec!["saved".to_owned()]));
    let l = d.lease(user).await;
    assert_eq!(
        l.abnormal_ends(d.clock.as_ref()()).len(),
        1,
        "the crash counts"
    );
}

#[tokio::test]
async fn reconcile_marks_dead_containers_gone_keeps_the_pool_and_asks_for_parks() {
    let d = Deployment::new(false).await;
    let r = d
        .router_with("0.1.0", |t| t.settings.containers.pool_target = 1)
        .await
        .unwrap();
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&r, None, "kept").await.0, 201);
    let fleet = d
        .fleet_with("0.1.0", true, &format!("https://{}", r.internal), |c| {
            c.containers.pool_target = 1;
        })
        .await;
    let mut pass = Reconciler::default();
    // Pool: one ready container beside the claimed one.
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    assert!(
        eventually(LIMIT, || async {
            d.count(ContainerState::Ready).await == 1
        })
        .await
    );
    // A dead claimed container is found gone, and its lease parked.
    let claimed = d.lease(user).await.container_id.unwrap();
    d.launcher
        .kill(&d.launcher.handle_of(claimed).unwrap(), true);
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    let l = d.lease(user).await;
    assert_eq!(l.state, LeaseState::Parked);
    assert_eq!(l.abnormal_ends(d.clock.as_ref()()).len(), 1);
    // Lifetime: a claimed container past its retirement is asked to park and does.
    assert_eq!(summaries(&r, None).await.0, 200);
    let id = d.lease(user).await.container_id.unwrap();
    let rec = d.containers().get(id).await.unwrap().unwrap();
    let mut old = rec.value.clone();
    old.retire_at_ms = 0;
    d.containers().put(&old, rec.cond()).await.unwrap();
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    assert_eq!(
        d.lease(user).await.park_requested(),
        Some(ParkReason::Lifetime)
    );
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    // Draining: a claimed container on a draining host is asked to park; a ready one stops.
    assert_eq!(summaries(&r, None).await.0, 200);
    let id = d.lease(user).await.container_id.unwrap();
    d.launcher.drain(&d.launcher.handle_of(id).unwrap());
    let ready: Vec<_> = d
        .records()
        .await
        .into_iter()
        .filter(|c| c.state == ContainerState::Ready)
        .collect();
    for c in &ready {
        d.launcher.drain(&d.launcher.handle_of(c.id).unwrap());
    }
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    assert_eq!(
        d.lease(user).await.park_requested(),
        Some(ParkReason::Drain)
    );
    for c in &ready {
        let rec = d.containers().get(c.id).await.unwrap().unwrap().value;
        assert_eq!(
            (rec.state, rec.stop_reason()),
            (ContainerState::Gone, Some(StopReason::Drain))
        );
    }
    // The write was killed with its first container before any snapshot: the store
    // loads, empty.
    assert_eq!(summaries(&r, None).await, (200, vec![]));
}

#[tokio::test]
async fn a_stuck_claim_is_resolved_and_a_hung_container_counts_as_failed() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    assert_eq!(summaries(&r, None).await.0, 200);
    // A router died after its claim call: the lease is still `claiming` a minute later.
    let read = d.leases().get(Slot::User(user)).await.unwrap().unwrap();
    let mut stuck = read.value.clone();
    stuck.set_state(LeaseState::Claiming, d.clock.as_ref()() - 61_000);
    d.leases()
        .put(Slot::User(user), &stuck, read.cond())
        .await
        .unwrap();
    let fleet = d
        .fleet("0.1.0", true, &format!("https://{}", r.internal))
        .await;
    let mut pass = Reconciler::default();
    d.lead(&fleet).await;
    fleet.pass(&mut pass).await.unwrap();
    assert_eq!(
        d.lease(user).await.state,
        LeaseState::Warm,
        "the container says it is claimed"
    );
    // A container that a router stopped as unresponsive counts as failed, whoever ends it.
    let id = d.lease(user).await.container_id.unwrap();
    assert_eq!(
        fleet
            .mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
            .await
            .unwrap(),
        Marked::Gone
    );
    assert_eq!(
        d.lease(user).await.abnormal_ends(d.clock.as_ref()()).len(),
        1
    );
}

#[tokio::test]
async fn rollout_between_two_builds_side_by_side() {
    let d = Deployment::new(false).await;
    let old = d
        .router_with("0.1.0", |t| {
            *t.one_host = false;
            t.settings.containers.pool_target = 1;
        })
        .await
        .unwrap();
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&old, None, "before").await.0, 201);
    let first = d.lease(user).await.container_id.unwrap();
    assert_eq!(
        d.containers()
            .get(first)
            .await
            .unwrap()
            .unwrap()
            .value
            .build,
        "0.1.0"
    );
    // A newer router starts beside it: it becomes `desired`, claims stay on the old build.
    d.launcher
        .set_runner(serve_runner("0.2.0", true, fleet_support::timings()));
    let new = d
        .router_with("0.2.0", |t| {
            *t.one_host = false;
            t.settings.containers.pool_target = 1;
        })
        .await
        .unwrap();
    let meta = Meta::new(d.db.clone());
    let record = meta.rollout().await.unwrap().unwrap().value;
    assert_eq!(
        (record.claim.build.as_str(), record.desired.build.as_str()),
        ("0.1.0", "0.2.0")
    );
    // An older router started now leaves the record alone.
    let mut again = d
        .router_with("0.1.0", |t| *t.one_host = false)
        .await
        .unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.desired.build,
        "0.2.0"
    );
    assert_eq!(again.stop(LIMIT).await, 0);
    // The new build's leader starts a desired-build container; once it is ready, claims
    // switch, and the old ready containers are stopped.
    let leader = d
        .fleet_with("0.2.0", false, &format!("https://{}", new.internal), |c| {
            c.containers.pool_target = 1;
        })
        .await;
    let mut pass = Reconciler::default();
    d.lead(&leader).await;
    leader.pass(&mut pass).await.unwrap();
    assert!(
        eventually(LIMIT, || async {
            d.records()
                .await
                .iter()
                .any(|c| c.build == "0.2.0" && c.state == ContainerState::Ready)
        })
        .await
    );
    d.lead(&leader).await;
    leader.pass(&mut pass).await.unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.claim.build,
        "0.2.0"
    );
    d.lead(&leader).await;
    leader.pass(&mut pass).await.unwrap();
    let old_ready = d
        .records()
        .await
        .iter()
        .filter(|c| c.build == "0.1.0" && c.state == ContainerState::Ready)
        .count();
    assert_eq!(old_ready, 0);
    // The user's container keeps its build until it parks; the next claim is the new
    // build, through either router, and an old router signs for the new container.
    assert_eq!(
        d.containers()
            .get(first)
            .await
            .unwrap()
            .unwrap()
            .value
            .state,
        ContainerState::Claimed
    );
    let c = d.containers().get(first).await.unwrap().unwrap().value;
    assert!(leader.park(first, peer(&c), ParkReason::Upgrade).await);
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    assert_eq!(save(&old, None, "after").await.0, 201);
    let second = d.lease(user).await.container_id.unwrap();
    assert_eq!(
        d.containers()
            .get(second)
            .await
            .unwrap()
            .unwrap()
            .value
            .build,
        "0.2.0"
    );
    let mut items = summaries(&new, None).await.1;
    items.sort();
    assert_eq!(items, vec!["after".to_owned(), "before".to_owned()]);
}

#[tokio::test]
async fn a_router_stopping_on_one_host_parks_its_containers_through_its_own_broker() {
    let d = Deployment::new(false).await;
    let mut r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&r, None, "stays").await.0, 201);
    assert_eq!(r.stop(Duration::from_secs(60)).await, 0);
    assert_eq!(d.lease(user).await.state, LeaseState::Parked);
    let r2 = d.router("0.1.0").await;
    assert_eq!(summaries(&r2, None).await.1, vec!["stays".to_owned()]);
}

#[tokio::test]
async fn a_stop_asks_hung_containers_at_once_and_leaves_no_task_behind() {
    // The wait is 110 seconds and each `park` call may take 10; this test shortens the wait
    // to 3 seconds, so asking one container after another would take over 30.
    const WAIT: Duration = Duration::from_secs(3);
    let d = Deployment::new(false).await;
    let control = Arc::new(StubControl::default());
    d.launcher
        .set_runner(stub_runner("0.1.0", true, control.clone()));
    let (start, url, internal) = d.router_start("0.1.0", |t| t.timing.shutdown_wait = WAIT);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    // The router runs on a runtime of its own, so that its tasks can be counted.
    let router = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let code = rt.block_on(run_router(start, async {
            let _ = stopped.await;
        }));
        let ended = Instant::now();
        let left = rt.block_on(async {
            let metrics = tokio::runtime::Handle::current().metrics();
            while metrics.num_alive_tasks() > 0 && ended.elapsed() < Duration::from_secs(2) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            metrics.num_alive_tasks()
        });
        (code, ended, left)
    });
    assert!(
        eventually(LIMIT, || async {
            reqwest::get(format!("{url}/healthz"))
                .await
                .is_ok_and(|r| r.status() == 200)
        })
        .await
    );
    // Three users with claimed containers that never answer `park`.
    let fleet = d.fleet("0.1.0", true, &format!("https://{internal}")).await;
    for _ in 0..3 {
        let user = UserId::from_uuid(Uuid::now_v7());
        let lease = fleet.acquire(Slot::User(user), user, LIMIT).await.unwrap();
        assert_eq!(lease.value.state, LeaseState::Warm);
    }
    control.hang_park.store(true, Ordering::Relaxed);
    let asked = Instant::now();
    stop.send(()).unwrap();
    let (code, ended, left) = tokio::task::spawn_blocking(move || router.join().unwrap())
        .await
        .unwrap();
    let took = ended - asked;
    assert_eq!(code, 0);
    assert!(
        took >= WAIT && took < WAIT + Duration::from_secs(3),
        "the stop took {took:?}"
    );
    assert_eq!(left, 0, "tasks outlived run_router");
    fleet.shutdown().await;
}

#[tokio::test]
async fn three_crashes_through_requests_pause_the_user() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&r, None, "x").await.0, 201);
    // Each crash is found by the next request, which a new container serves.
    for _ in 0..2 {
        let id = d.lease(user).await.container_id.unwrap();
        d.launcher.kill(&d.launcher.handle_of(id).unwrap(), false);
        assert_eq!(summaries(&r, None).await.0, 200);
    }
    let id = d.lease(user).await.container_id.unwrap();
    d.launcher.kill(&d.launcher.handle_of(id).unwrap(), false);
    // The third: the user is paused, this request and the next.
    for _ in 0..2 {
        let answer = r
            .req(reqwest::Method::GET, "/v1/memories")
            .send()
            .await
            .unwrap();
        assert_eq!(answer.headers()["retry-after"], "60");
        let (status, v) = json(answer).await;
        assert_eq!(
            (
                status,
                v["error"]["code"].as_str(),
                v["error"]["reason"].as_str()
            ),
            (503, Some("unavailable"), Some("crashed"))
        );
    }
    assert_eq!(
        d.lease(user).await.abnormal_ends(d.clock.as_ref()()).len(),
        3
    );
}

#[tokio::test]
async fn a_long_import_through_the_router_is_not_cut_by_the_request_timeout() {
    // The case is a 40-second import; this test runs a 10-second one against a
    // request timeout of 3 seconds and the fleet's 2 seconds of silence before `status`.
    let d = Deployment::new(false).await;
    let r = d
        .router_with("0.1.0", |t| t.edge.limits.request_timeout_ms = 3_000)
        .await
        .unwrap();
    assert_eq!(save(&r, None, "exported").await.0, 201);
    let text = r
        .req(reqwest::Method::GET, "/v1/export")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let lines: Vec<String> = text.lines().map(|l| format!("{l}\n")).collect();
    assert!(lines.len() >= 2, "{text}");
    // The header at once, then one line a second for 10 seconds.
    let body = futures_util::stream::iter(0..11).then(move |i| {
        let line = if i == 0 {
            lines[0].clone()
        } else {
            lines[1 + (i - 1) % (lines.len() - 1)].clone()
        };
        async move {
            if i > 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok::<_, std::io::Error>(line)
        }
    });
    let started = Instant::now();
    let answer = r
        .req(reqwest::Method::POST, "/v1/import")
        .header("content-type", "application/x-ndjson")
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(10));
    let (status, v) = json(answer).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["errors"], serde_json::json!([]), "{v}");
    let (user, _) = local_user(&d).await;
    let lease = d.lease(user).await;
    assert_eq!(
        lease.state,
        LeaseState::Warm,
        "the container was not marked gone"
    );
    assert!(lease.abnormal_ends(d.clock.as_ref()()).is_empty());
}

#[tokio::test]
async fn a_container_whose_hello_names_the_other_test_mode_is_stopped() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    d.launcher
        .set_runner(serve_runner("0.1.0", false, fleet_support::timings()));
    let fleet = d
        .fleet("0.1.0", true, &format!("https://{}", r.internal))
        .await;
    assert!(
        fleet
            .launch(BuildSpec {
                build: "0.1.0".into(),
                spec: String::new(),
            })
            .await
            .unwrap()
    );
    assert!(
        eventually(LIMIT, || async {
            d.records().await.iter().any(|c| {
                c.state == ContainerState::Gone && c.stop_reason() == Some(StopReason::LaunchFailed)
            })
        })
        .await
    );
    assert!(eventually(LIMIT, || async { d.launcher.running() == 0 }).await);
    assert_eq!(d.count(ContainerState::Ready).await, 0);
}

#[tokio::test]
async fn a_container_whose_parked_call_fails_still_exits_and_the_loop_frees_its_lease() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    assert_eq!(save(&r, None, "kept").await.0, 201);
    let id = d.lease(user).await.container_id.unwrap();
    let fleet = d
        .fleet("9.0.0", true, &format!("https://{}", r.internal))
        .await;
    // The snapshot goes through; the router cannot record the container's end.
    *d.flaky.fail.lock().unwrap() = Some("containers");
    let c = d.containers().get(id).await.unwrap().unwrap().value;
    assert!(fleet.park(id, peer(&c), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async { d.launcher.running() == 0 }).await,
        "the container exited after its tries"
    );
    assert_eq!(d.lease(user).await.state, LeaseState::Warm);
    *d.flaky.fail.lock().unwrap() = None;
    d.lead(&fleet).await;
    fleet.pass(&mut Reconciler::default()).await.unwrap();
    let l = d.lease(user).await;
    assert_eq!(l.state, LeaseState::Parked);
    assert!(
        l.abnormal_ends(d.clock.as_ref()()).is_empty(),
        "it ended cleanly"
    );
    assert_eq!(summaries(&r, None).await.1, vec!["kept".to_owned()]);
}

#[tokio::test]
async fn a_container_that_lost_its_lease_exits_and_its_late_upload_hides_nothing() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    let (user, _) = local_user(&d).await;
    // The first container holds an acknowledged write it has not saved.
    assert_eq!(save(&r, None, "old").await.0, 201);
    let first = d.lease(user).await.container_id.unwrap();
    let old = d.containers().get(first).await.unwrap().unwrap().value;
    // The launcher loses sight of it, so the lease passes on while it still runs.
    d.launcher.forget(&d.launcher.handle_of(first).unwrap());
    let fleet = d
        .fleet("9.0.0", true, &format!("https://{}", r.internal))
        .await;
    assert_eq!(
        fleet.mark_gone(first, None, Caller::Request).await.unwrap(),
        Marked::Gone
    );
    // A second container takes the user, writes and saves.
    assert_eq!(save(&r, None, "newer").await.0, 201);
    let second = d.lease(user).await.container_id.unwrap();
    assert_ne!(second, first);
    let c = d.containers().get(second).await.unwrap().unwrap().value;
    assert!(fleet.park(second, peer(&c), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async {
            d.lease(user).await.state == LeaseState::Parked
        })
        .await
    );
    // Now the first one tries to save: it gets lease_lost and exits.
    assert!(fleet.park(first, peer(&old), ParkReason::Restore).await);
    assert!(
        eventually(LIMIT, || async { d.launcher.running() == 0 }).await,
        "the first container exited"
    );
    assert_eq!(summaries(&r, None).await.1, vec!["newer".to_owned()]);
}
