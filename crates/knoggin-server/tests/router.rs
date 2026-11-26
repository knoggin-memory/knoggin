// SPDX-License-Identifier: AGPL-3.0-only
//! The router's lease, claim, "Mark gone", pool, reconcile, rollout and forwarding rules
//! (tab "Server", Tests that must exist), run against stub containers that the fake
//! launcher starts in this process, over one shared in-memory state database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http::{StatusCode, header};
use http_body_util::BodyExt;
use knoggin_core::{StoreId, UserId};
use knoggin_http::ctx::{CredentialKind, CredentialRef, Permissions};
use knoggin_http::{Code, RequestContext};
use knoggin_server::launcher::{BuildInfo, LogLevel};
use knoggin_server::lease::{Caller, Fleet, FleetConfig, Marked};
use knoggin_server::link::api::{ClaimLimits, ClaimSettings};
use knoggin_server::link::{InternalSecret, ServerIdentity};
use knoggin_server::reconcile::Reconciler;
use knoggin_server::router::forward::{Timing, forwarder};
use knoggin_server::settings::ContainerSettings;
use knoggin_server::storage::MemoryStorage;
use knoggin_server::testing::{Behaviour, FakeLauncher, StubControl, stub_runner};
use knoggin_state::record::{BuildSpec, ContainerState, Lease, LeaseState, ParkReason, StopReason};
use knoggin_state::testing::{TestClock, test_field_keys};
use knoggin_state::{Containers, Leases, LocalFileDb, Meta, StateDb};
use tower::ServiceExt;
use uuid::Uuid;

const BUILD: &str = "0.1.0";

struct World {
    clock: TestClock,
    db: Arc<dyn StateDb>,
    launcher: Arc<FakeLauncher>,
    control: Arc<StubControl>,
    storage: Arc<MemoryStorage>,
}

impl World {
    async fn new() -> Self {
        let clock = TestClock::new();
        clock.advance(1_800_000_000_000);
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
        let control = Arc::new(StubControl::default());
        let launcher = FakeLauncher::new(stub_runner(BUILD, true, control.clone()), clock.clock());
        let storage = Arc::new(MemoryStorage::start(clock.clock()).await.unwrap());
        Self {
            clock,
            db,
            launcher,
            control,
            storage,
        }
    }

    fn fleet(&self, build: &str, one_host: bool, f: impl FnOnce(&mut FleetConfig)) -> Arc<Fleet> {
        let pin = ServerIdentity::for_container(Uuid::now_v7()).unwrap().pin();
        let mut config = FleetConfig {
            router_id: Uuid::now_v7(),
            build: BuildInfo::new(build),
            spec: String::new(),
            one_host,
            test_mode: true,
            containers: ContainerSettings {
                pool_target: 0,
                claim_wait_ms: 8_000,
                ..ContainerSettings::default()
            },
            claim: ClaimSettings {
                idle_seconds: 600,
                memory_limit_bytes: 1 << 30,
                public_url: "http://127.0.0.1:1".into(),
                source_url: "https://example.invalid".into(),
                limits: ClaimLimits {
                    body_bytes: 10 << 20,
                    request_timeout_ms: 30_000,
                    store_concurrency: 64,
                },
            },
            secret: InternalSecret::from_bytes([7u8; 32]),
            router_url: "https://127.0.0.1:1".into(),
            router_pin: pin,
            log_level: LogLevel::Info,
            allow_downgrade: false,
        };
        f(&mut config);
        Fleet::new(
            config,
            self.db.clone(),
            Arc::new(test_field_keys()),
            self.launcher.clone(),
            self.storage.clone(),
            self.clock.clock(),
        )
        .unwrap()
    }

    fn leases(&self) -> Leases {
        Leases::new(self.db.clone())
    }

    fn containers(&self) -> Containers {
        Containers::new(self.db.clone())
    }

    async fn lease(&self, user: UserId) -> Lease {
        self.leases()
            .get(user)
            .await
            .unwrap()
            .map(|l| l.value)
            .unwrap_or_default()
    }

    async fn count(&self, state: ContainerState) -> usize {
        self.containers()
            .list()
            .await
            .unwrap()
            .iter()
            .filter(|c| c.value.state == state)
            .count()
    }
}

fn user() -> UserId {
    UserId::from_uuid(Uuid::now_v7())
}

async fn warm(fleet: &Arc<Fleet>, user: UserId) -> Uuid {
    let l = fleet.acquire(user, Duration::from_secs(8)).await.unwrap();
    assert_eq!(l.value.state, LeaseState::Warm);
    l.value.container_id.unwrap()
}

async fn wait_ready(world: &World, n: usize) {
    for _ in 0..80 {
        if world.count(ContainerState::Ready).await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no ready container");
}

#[tokio::test]
async fn two_routers_race_to_claim_one_user_and_both_are_served() {
    let w = World::new().await;
    let (a, b) = (w.fleet(BUILD, true, |_| {}), w.fleet(BUILD, true, |_| {}));
    let u = user();
    let (x, y) = tokio::join!(
        a.acquire(u, Duration::from_secs(8)),
        b.acquire(u, Duration::from_secs(8))
    );
    let (x, y) = (x.unwrap(), y.unwrap());
    assert_eq!(x.value.container_id, y.value.container_id);
    let lease = w.lease(u).await;
    assert_eq!(lease.claims(w.clock.now()), 1, "exactly one claim won");
    assert_eq!(w.count(ContainerState::Claimed).await, 1);
}

#[tokio::test]
async fn two_routers_claim_one_ready_container_for_two_users_and_the_loser_keeps_it() {
    let w = World::new().await;
    let (a, b) = (w.fleet(BUILD, true, |_| {}), w.fleet(BUILD, true, |_| {}));
    assert!(
        a.launch(BuildSpec {
            build: BUILD.into(),
            spec: String::new()
        })
        .await
        .unwrap()
    );
    wait_ready(&w, 1).await;
    w.launcher.refuse.store(true, Ordering::Relaxed);
    let (u1, u2) = (user(), user());
    let (x, y) = tokio::join!(
        a.acquire(u1, Duration::from_secs(2)),
        b.acquire(u2, Duration::from_secs(2))
    );
    assert!(x.is_ok() != y.is_ok(), "exactly one user got the container");
    let named = [w.lease(u1).await, w.lease(u2).await]
        .iter()
        .filter(|l| l.state != LeaseState::Parked)
        .count();
    assert_eq!(named, 1);
    assert_eq!(
        w.count(ContainerState::Claimed).await,
        1,
        "the loser did not mark it gone"
    );
}

#[tokio::test]
async fn a_failed_claim_marks_the_container_gone_and_does_not_count_against_the_user() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |c| c.containers.claim_wait_ms = 1_500);
    w.control.refuse_claim.store(true, Ordering::Relaxed);
    let u = user();
    let e = f
        .acquire(u, Duration::from_millis(1_500))
        .await
        .unwrap_err();
    assert_eq!(e.code, Code::Unavailable);
    let lease = w.lease(u).await;
    assert_eq!(lease.claims(w.clock.now()), 0);
    assert!(lease.abnormal_ends(w.clock.now()).is_empty());
    let gone = w.containers().list().await.unwrap();
    assert!(
        gone.iter()
            .any(|c| c.value.stop_reason() == Some(StopReason::ClaimFailed))
    );
}

#[tokio::test]
async fn a_claim_after_the_generation_was_set_back_still_rises_above_the_clock() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let mut back = Lease::default();
    back.generation = 3;
    w.leases()
        .put(u, &back, knoggin_state::Cond::Absent)
        .await
        .unwrap();
    warm(&f, u).await;
    let g = w.lease(u).await.generation;
    assert!(g >= u64::try_from(w.clock.now() / 1000).unwrap(), "{g}");
}

#[tokio::test]
async fn three_abnormal_ends_pause_the_user_and_the_pause_ends_by_itself() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    for memory in [false, false, true] {
        let id = warm(&f, u).await;
        w.launcher.kill(&w.launcher.handle_of(id).unwrap(), memory);
        assert_eq!(
            f.mark_gone(id, None, Caller::Request).await.unwrap(),
            Marked::Gone
        );
    }
    let e = f.acquire(u, Duration::from_secs(1)).await.unwrap_err();
    assert_eq!(
        (e.code, e.reason, e.retry_after),
        (Code::Unavailable, Some("out_of_memory"), Some(60))
    );
    w.clock.advance(600_001);
    warm(&f, u).await;
}

#[tokio::test]
async fn a_hung_container_stopped_by_a_router_counts_as_an_abnormal_end() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let id = warm(&f, u).await;
    // The stop ends it cleanly, but the record's reason decides.
    let m = f
        .mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
        .await
        .unwrap();
    assert_eq!(m, Marked::Gone);
    let ends = w.lease(u).await.abnormal_ends(w.clock.now());
    assert_eq!(ends.len(), 1);
}

#[tokio::test]
async fn a_container_that_parks_after_every_request_caps_its_user_at_the_seventh_claim() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    for _ in 0..6 {
        let id = warm(&f, u).await;
        let c = w.containers().get(id).await.unwrap().unwrap().value;
        let peer = knoggin_server::link::Peer {
            address: c.address.unwrap().parse().unwrap(),
            pin: c.cert_sha256.unwrap().parse().unwrap(),
        };
        assert!(f.park(id, peer, ParkReason::Lifetime).await);
        f.finish_parked(id).await.unwrap();
        assert_eq!(w.lease(u).await.state, LeaseState::Parked);
    }
    let e = f.acquire(u, Duration::from_secs(1)).await.unwrap_err();
    assert_eq!(
        (e.code, e.reason, e.retry_after),
        (Code::Unavailable, Some("crashed"), Some(60))
    );
    warm(&f, user()).await;
}

#[tokio::test]
async fn with_the_launcher_unreachable_mark_gone_frees_no_lease() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let id = warm(&f, u).await;
    w.launcher.unreachable.store(true, Ordering::Relaxed);
    assert!(
        f.mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
            .await
            .is_err()
    );
    assert_eq!(w.lease(u).await.state, LeaseState::Warm);
    let c = w.containers().get(id).await.unwrap().unwrap().value;
    assert_eq!(
        c.stop_reason(),
        None,
        "the launcher could not even be asked"
    );
    // A failed stop keeps the reason; the loop asks again.
    w.launcher.unreachable.store(false, Ordering::Relaxed);
    w.launcher.fail_stop.store(true, Ordering::Relaxed);
    assert!(
        f.mark_gone(id, Some(StopReason::Unresponsive), Caller::Request)
            .await
            .is_err()
    );
    assert_eq!(w.lease(u).await.state, LeaseState::Warm);
    let c = w.containers().get(id).await.unwrap().unwrap().value;
    assert_eq!(c.stop_reason(), Some(StopReason::Unresponsive));
    w.launcher.fail_stop.store(false, Ordering::Relaxed);
    let mut r = Reconciler::default();
    f.take_leadership().await.unwrap();
    f.pass(&mut r).await.unwrap();
    assert_eq!(w.launcher.running(), 0, "the loop repeated the stop");
    f.pass(&mut r).await.unwrap();
    assert_eq!(w.lease(u).await.state, LeaseState::Parked);
}

#[tokio::test]
async fn reconcile_resolves_stuck_claims_and_leaves_a_launch_in_progress_alone() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let id = warm(&f, u).await;
    // A router that died after the claim call: the lease stays claiming.
    let read = w.leases().get(u).await.unwrap().unwrap();
    let mut stuck = read.value.clone();
    stuck.set_state(LeaseState::Claiming, w.clock.now());
    w.leases().put(u, &stuck, read.cond()).await.unwrap();
    // A launch in progress: a starting record of a moment ago.
    let starting = knoggin_state::record::Container::starting(
        Uuid::now_v7(),
        BUILD.into(),
        String::new(),
        w.clock.now(),
        i64::MAX,
    );
    w.containers()
        .put(&starting, knoggin_state::Cond::Absent)
        .await
        .unwrap();
    let mut r = Reconciler::default();
    assert!(f.take_leadership().await.unwrap());
    f.pass(&mut r).await.unwrap();
    assert_eq!(
        w.lease(u).await.state,
        LeaseState::Claiming,
        "not yet 60 seconds"
    );
    w.clock.advance(61_000);
    f.pass(&mut r).await.unwrap();
    let l = w.lease(u).await;
    assert_eq!((l.state, l.container_id), (LeaseState::Warm, Some(id)));
    let s = w
        .containers()
        .get(starting.id)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(
        s.state,
        ContainerState::Starting,
        "a launch in progress is not touched"
    );
    w.clock.advance(120_000);
    f.pass(&mut r).await.unwrap();
    let s = w
        .containers()
        .get(starting.id)
        .await
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(s.state, ContainerState::Gone);
}

#[tokio::test]
async fn a_container_the_launcher_stops_listing_is_marked_gone_on_the_second_pass() {
    let w = World::new().await;
    let f = w.fleet(BUILD, false, |_| {});
    Meta::new(w.db.clone())
        .put_rollout(
            &knoggin_state::record::Rollout {
                claim: BuildSpec {
                    build: BUILD.into(),
                    spec: String::new(),
                },
                desired: BuildSpec {
                    build: BUILD.into(),
                    spec: String::new(),
                },
                retire_before_ms: None,
            },
            knoggin_state::Cond::Absent,
        )
        .await
        .unwrap();
    let u = user();
    let id = warm(&f, u).await;
    w.launcher.forget(&w.launcher.handle_of(id).unwrap());
    // On ECS the request path leaves an unknown task alone.
    assert_eq!(
        f.mark_gone(id, None, Caller::Request).await.unwrap(),
        Marked::Unknown
    );
    let mut r = Reconciler::default();
    assert!(f.take_leadership().await.unwrap());
    f.pass(&mut r).await.unwrap();
    assert_eq!(
        w.lease(u).await.state,
        LeaseState::Warm,
        "one pass is not enough"
    );
    f.pass(&mut r).await.unwrap();
    let l = w.lease(u).await;
    assert_eq!(l.state, LeaseState::Parked);
    assert_eq!(l.abnormal_ends(w.clock.now()).len(), 1, "counted as failed");
}

#[tokio::test]
async fn the_pool_is_kept_at_its_target_and_strays_are_stopped() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |c| c.containers.pool_target = 2);
    let mut r = Reconciler::default();
    assert!(f.take_leadership().await.unwrap());
    f.pass(&mut r).await.unwrap();
    wait_ready(&w, 2).await;
    // A container no record names is a stray.
    let other = w.fleet(BUILD, true, |_| {});
    other
        .launch(BuildSpec {
            build: BUILD.into(),
            spec: String::new(),
        })
        .await
        .unwrap();
    wait_ready(&w, 3).await;
    let records = w.containers().list().await.unwrap();
    let newest = records
        .iter()
        .max_by_key(|c| c.value.started_ms)
        .unwrap()
        .value
        .id;
    let h = w.launcher.handle_of(newest).unwrap();
    w.containers()
        .delete(newest, knoggin_state::Cond::Any)
        .await
        .unwrap();
    f.pass(&mut r).await.unwrap();
    assert_eq!(w.launcher.running(), 2);
    assert!(!w.launcher.handles().is_empty());
    let _ = h;
}

#[tokio::test]
async fn an_older_router_takes_the_lease_only_when_no_newer_router_holds_it() {
    let w = World::new().await;
    let new = w.fleet("0.2.0", true, |_| {});
    let old = w.fleet("0.1.0", true, |_| {});
    assert!(new.take_leadership().await.unwrap());
    assert!(!old.take_leadership().await.unwrap());
    // The old router led first: a newer one takes over at once.
    let w2 = World::new().await;
    let (old2, new2) = (
        w2.fleet("0.1.0", true, |_| {}),
        w2.fleet("0.2.0", true, |_| {}),
    );
    assert!(old2.take_leadership().await.unwrap());
    assert!(new2.take_leadership().await.unwrap());
    assert!(!old2.take_leadership().await.unwrap());
    // When the newer router stops renewing, the old one leads again within 30 seconds.
    w2.clock.advance(30_001);
    assert!(old2.take_leadership().await.unwrap());
}

#[tokio::test]
async fn the_rollout_record_at_start() {
    let w = World::new().await;
    let meta = Meta::new(w.db.clone());
    let spec = |b: &str, s: &str| BuildSpec {
        build: b.into(),
        spec: s.into(),
    };
    knoggin_server::rollout::at_start(&meta, &spec("0.1.0", "td:1"), false)
        .await
        .unwrap();
    let r = meta.rollout().await.unwrap().unwrap().value;
    assert_eq!(
        (r.claim.build.as_str(), r.desired.build.as_str()),
        ("0.1.0", "0.1.0")
    );
    knoggin_server::rollout::at_start(&meta, &spec("0.2.0", "td:2"), false)
        .await
        .unwrap();
    let r = meta.rollout().await.unwrap().unwrap().value;
    assert_eq!(
        (r.claim.build.as_str(), r.desired.build.as_str()),
        ("0.1.0", "0.2.0")
    );
    // An older router leaves the record alone, unless downgrades are allowed.
    knoggin_server::rollout::at_start(&meta, &spec("0.1.0", "td:1"), false)
        .await
        .unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.desired.build,
        "0.2.0"
    );
    knoggin_server::rollout::at_start(&meta, &spec("0.1.0", "td:1"), true)
        .await
        .unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.desired.build,
        "0.1.0"
    );
    // The same build with another spec becomes desired.
    knoggin_server::rollout::at_start(&meta, &spec("0.1.0", "td:9"), false)
        .await
        .unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.desired.spec,
        "td:9"
    );
}

#[tokio::test]
async fn claims_switch_only_after_a_new_build_container_is_ready() {
    let w = World::new().await;
    let meta = Meta::new(w.db.clone());
    let spec = |b: &str| BuildSpec {
        build: b.into(),
        spec: format!("td:{b}"),
    };
    knoggin_server::rollout::at_start(&meta, &spec("0.1.0"), false)
        .await
        .unwrap();
    let old = w.fleet("0.1.0", false, |c| {
        c.spec = "td:0.1.0".into();
        c.containers.pool_target = 1
    });
    let mut r = Reconciler::default();
    assert!(old.take_leadership().await.unwrap());
    old.pass(&mut r).await.unwrap();
    wait_ready(&w, 1).await;
    // A new router whose containers never become ready (they report the old build's
    // test mode the wrong way) does not take over claims.
    knoggin_server::rollout::at_start(&meta, &spec("0.2.0"), false)
        .await
        .unwrap();
    let new = w.fleet("0.2.0", false, |c| {
        c.spec = "td:0.2.0".into();
        c.containers.pool_target = 1
    });
    w.launcher
        .set_runner(stub_runner("0.2.0", false, w.control.clone()));
    let mut r2 = Reconciler::default();
    assert!(new.take_leadership().await.unwrap());
    new.pass(&mut r2).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    new.pass(&mut r2).await.unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.claim.build,
        "0.1.0"
    );
    // A good build becomes ready and claims switch to it.
    w.launcher
        .set_runner(stub_runner("0.2.0", true, w.control.clone()));
    w.clock.advance(61_000);
    new.pass(&mut r2).await.unwrap();
    for _ in 0..50 {
        if w.containers()
            .list()
            .await
            .unwrap()
            .iter()
            .any(|c| c.value.build == "0.2.0" && c.value.state == ContainerState::Ready)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    new.pass(&mut r2).await.unwrap();
    assert_eq!(
        meta.rollout().await.unwrap().unwrap().value.claim.build,
        "0.2.0"
    );
    // The old build's ready container is stopped by the pool step.
    new.pass(&mut r2).await.unwrap();
    let old_ready = w
        .containers()
        .list()
        .await
        .unwrap()
        .iter()
        .filter(|c| c.value.build == "0.1.0" && c.value.state == ContainerState::Ready)
        .count();
    assert_eq!(old_ready, 0);
}

#[tokio::test]
async fn retire_before_parks_in_batches_of_a_tenth() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let mut users = Vec::new();
    for _ in 0..12 {
        let u = user();
        warm(&f, u).await;
        users.push(u);
    }
    let meta = Meta::new(w.db.clone());
    let own = BuildSpec {
        build: BUILD.into(),
        spec: String::new(),
    };
    knoggin_server::rollout::at_start(&meta, &own, false)
        .await
        .unwrap();
    let r = meta.rollout().await.unwrap().unwrap();
    let mut next = r.value.clone();
    next.retire_before_ms = Some(w.clock.now() + 1);
    meta.put_rollout(&next, r.cond()).await.unwrap();
    w.control.exit_on_park.store(false, Ordering::Relaxed);
    let mut rec = Reconciler::default();
    assert!(f.take_leadership().await.unwrap());
    f.pass(&mut rec).await.unwrap();
    let mut asked = 0;
    for u in &users {
        if w.lease(*u).await.park_requested() == Some(ParkReason::Upgrade) {
            asked += 1;
        }
    }
    assert_eq!(asked, 1, "12 leases: a tenth, at least one");
}

// ---- Forwarding -----------------------------------------------------------------------

fn ctx(user: UserId) -> RequestContext {
    RequestContext {
        request_id: Uuid::now_v7(),
        user_id: user,
        store_id: StoreId::from_uuid(Uuid::now_v7()),
        permissions: Permissions::READ_WRITE,
        credential: CredentialRef {
            kind: CredentialKind::None,
            id: None,
        },
        default_namespace: None,
        client: None,
    }
}

async fn send(f: &Arc<Fleet>, user: UserId, request: Request) -> Response {
    let timing = Timing {
        silence: Duration::from_millis(500),
        events_silence: Duration::from_millis(500),
        status_limit: Duration::from_millis(300),
    };
    let mut request = request;
    request.extensions_mut().insert(ctx(user));
    forwarder(
        f.clone(),
        Duration::from_secs(if timeout_short(&request) { 2 } else { 20 }),
        timing,
    )
    .oneshot(request)
    .await
    .unwrap()
}

/// The hanging-request test wants the limit short; the others want room for a resend.
fn timeout_short(r: &Request) -> bool {
    r.headers().contains_key("x-short-limit")
}

async fn body_json(r: Response) -> serde_json::Value {
    let b = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn a_forwarded_request_carries_no_client_credential_and_comes_back_filtered() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let r = send(
        &f,
        u,
        Request::post("/v1/memories?x=1")
            .header(header::AUTHORIZATION, "Bearer client-secret")
            .header(header::COOKIE, "__Host-session=abc")
            .header(header::CONNECTION, "knoggin-user-id")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers().get("x-not-passed").is_none());
    assert_eq!(r.headers().get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(
        r.headers().get(header::CONTENT_SECURITY_POLICY).unwrap(),
        "default-src 'none'; sandbox"
    );
    let v = body_json(r).await;
    assert_eq!(v["path"], "/v1/memories");
    let seen = w.control.seen.lock().unwrap().pop().unwrap();
    assert!(seen.get(header::COOKIE).is_none());
    assert_ne!(
        seen.get(header::AUTHORIZATION).unwrap(),
        "Bearer client-secret"
    );
    assert_eq!(
        seen.get("knoggin-user-id").unwrap().to_str().unwrap(),
        u.to_string()
    );
}

#[tokio::test]
async fn a_hostile_container_cannot_speak_to_the_client() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    warm(&f, u).await;
    let get = || Request::get("/v1/tags").body(Body::empty()).unwrap();
    *w.control.behaviour.lock().unwrap() = Behaviour::Hostile;
    let r = send(&f, u, get()).await;
    assert_eq!(r.status(), StatusCode::OK);
    for h in [
        header::SET_COOKIE,
        header::LOCATION,
        header::WWW_AUTHENTICATE,
    ] {
        assert!(r.headers().get(&h).is_none(), "{h}");
    }
    for b in [
        Behaviour::Html,
        Behaviour::Redirect,
        Behaviour::Unauthorized,
    ] {
        *w.control.behaviour.lock().unwrap() = b;
        let r = send(&f, u, get()).await;
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR, "{b:?}");
    }
}

#[tokio::test]
async fn the_router_answers_the_event_stream_and_mcp_without_waking_anything() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let r = send(
        &f,
        u,
        Request::get("/v1/ui/events").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = send(&f, u, Request::get("/mcp").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.headers().get(header::ALLOW).unwrap(), "POST, DELETE");
    let r = send(&f, u, Request::delete("/mcp").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(w.launcher.handles().len(), 0, "nothing was launched");
}

#[tokio::test]
async fn an_ordinary_request_past_its_limit_times_out_and_the_container_stays() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let id = warm(&f, u).await;
    *w.control.behaviour.lock().unwrap() = Behaviour::Hang;
    let r = send(
        &f,
        u,
        Request::get("/v1/tags")
            .header("x-short-limit", "1")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body_json(r).await["error"]["code"], "timeout");
    assert_eq!(
        w.containers().get(id).await.unwrap().unwrap().value.state,
        ContainerState::Claimed
    );
}

#[tokio::test]
async fn a_container_that_stops_answering_status_is_marked_gone_and_a_get_is_sent_once_more() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let first = warm(&f, u).await;
    *w.control.behaviour.lock().unwrap() = Behaviour::Dead;
    // The resend reaches a new container, which is also dead: answered unavailable.
    let r = send(&f, u, Request::get("/v1/tags").body(Body::empty()).unwrap()).await;
    let v = body_json(r).await;
    assert_eq!(v["error"]["code"], "unavailable");
    let c = w.containers().get(first).await.unwrap().unwrap().value;
    assert_eq!(
        (c.state, c.stop_reason()),
        (ContainerState::Gone, Some(StopReason::Unresponsive))
    );
    let ends = w.lease(u).await.abnormal_ends(w.clock.now());
    assert_eq!(ends.len(), 2, "sent once more, at most");
}

#[tokio::test]
async fn a_killed_container_is_found_gone_and_a_get_is_served_by_a_new_one() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |_| {});
    let u = user();
    let first = warm(&f, u).await;
    w.launcher
        .kill(&w.launcher.handle_of(first).unwrap(), false);
    let r = send(&f, u, Request::get("/v1/tags").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_ne!(v["container"].as_str().unwrap(), first.to_string());
    // A write without an idempotency key is not sent again.
    let second: Uuid = v["container"].as_str().unwrap().parse().unwrap();
    w.launcher
        .kill(&w.launcher.handle_of(second).unwrap(), false);
    let r = send(
        &f,
        u,
        Request::post("/v1/memories")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(body_json(r).await["error"]["code"], "unavailable");
}

#[tokio::test]
async fn a_parking_container_makes_the_request_wait() {
    let w = World::new().await;
    let f = w.fleet(BUILD, true, |c| c.containers.claim_wait_ms = 1_000);
    let u = user();
    warm(&f, u).await;
    *w.control.behaviour.lock().unwrap() = Behaviour::Parking;
    let started = std::time::Instant::now();
    let r = send(&f, u, Request::get("/v1/tags").body(Body::empty()).unwrap()).await;
    assert_eq!(body_json(r).await["error"]["code"], "unavailable");
    assert!(started.elapsed() >= Duration::from_millis(700));
}

#[tokio::test]
async fn every_store_route_pattern_is_one_the_store_layer_serves() {
    let dir = tempfile::tempdir().unwrap();
    let host = knoggin_http::testing::TestHost::new(
        dir.path(),
        knoggin_http::testing::fake_config(),
        true,
        knoggin_http::host::EmbedWaits::default(),
    )
    .await;
    let layer = knoggin_http::store_layer(host.host.clone());
    // Without a request context a route of the store layer answers `internal` (or 405
    // for another method); a path it does not serve answers 404.
    for p in knoggin_server::router::forward::STORE_ROUTES {
        let path = p
            .replace("{id}", &Uuid::now_v7().to_string())
            .replace("{name}", "default");
        let r = layer
            .clone()
            .oneshot(Request::get(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(
            r.status(),
            StatusCode::NOT_FOUND,
            "{path} is no store route"
        );
    }
    let r = layer
        .oneshot(Request::get("/v1/nothing").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}
