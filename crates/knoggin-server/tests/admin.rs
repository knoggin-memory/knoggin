// SPDX-License-Identifier: AGPL-3.0-only
//! The admin commands on a server (restore, the bucket self-test, `retire-before` and
//! master-key rotation), in process: the state file in
//! memory, `MemoryStorage`, and for the restore the real container role and broker.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use http::{Method, StatusCode};
use knoggin_core::snapshot::SnapshotName;
use knoggin_core::{Sensitive, StoreId, Timestamp, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_crypto::source::{FileRule, create_key_file, read_key_file};
use knoggin_server::admin::{
    Admin, BucketStep, RestoreOutcome, RetireBefore, bucket_selftest, rotate_master_key, selftest,
};
use knoggin_server::storage::{Fault, MemoryStorage, Weakness, names};
use knoggin_state::record::{BuildSpec, LeaseState, ParkReason};
use knoggin_state::testing::TestClock;
use knoggin_state::{LocalFileDb, Meta, NewStore, NewUser, StateDb, Stores, Users};
use support::{Options, Rig, eventually, quick, settings};

const GIB: u64 = 1 << 30;
const SECONDS: Duration = Duration::from_secs(10);

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn admin_of(rig: &Rig) -> Admin {
    Admin::new(
        rig.db.clone(),
        Arc::new(knoggin_state::testing::test_field_keys()),
        rig.storage.clone(),
        rig.clock.clock(),
    )
}

fn summaries(body: &[u8]) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_slice(body).unwrap();
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["summary"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_bucket_self_test_passes_and_fails_at_each_broken_property() {
    let clock = TestClock::new();
    let storage = MemoryStorage::start(clock.clock()).await.unwrap();
    let now = || Timestamp::from_millis(clock.now());
    bucket_selftest(&storage, &http(), now()).await.unwrap();
    // A run leaves only its own uploads, under `selftest/`.
    assert!(
        storage
            .keys()
            .iter()
            .all(|k| k.starts_with(names::SELFTEST))
    );
    assert_eq!(storage.keys().len(), 1);

    storage.set_fault(Fault::Fail).await.unwrap();
    assert_eq!(
        bucket_selftest(&storage, &http(), now())
            .await
            .unwrap_err()
            .step,
        BucketStep::Upload
    );
    storage.set_fault(Fault::None).await.unwrap();
    for (weakness, step) in [
        (Weakness::Overwrites, BucketStep::CreateOnly),
        (Weakness::Unsigned, BucketStep::Signed),
        (Weakness::NoExpiry, BucketStep::Expiry),
        (Weakness::StaleListing, BucketStep::Listing),
    ] {
        storage.weaken(Some(weakness));
        let failure = bucket_selftest(&storage, &http(), now()).await.unwrap_err();
        assert_eq!(failure.step, step, "{weakness:?}: {failure}");
        storage.weaken(None);
    }
    bucket_selftest(&storage, &http(), now()).await.unwrap();
}

#[tokio::test]
async fn the_admin_self_test_checks_the_database_the_key_provider_and_the_bucket() {
    let clock = TestClock::new();
    let storage = MemoryStorage::start(clock.clock()).await.unwrap();
    let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
    let keys = knoggin_crypto::testing::test_provider();
    let now = Timestamp::from_millis(clock.now());
    let report = selftest(db.clone(), &keys, &storage, &http(), now).await;
    assert!(report.passed(), "{report:?}");
    storage.weaken(Some(Weakness::NoExpiry));
    let report = selftest(db, &keys, &storage, &http(), now).await;
    assert!(!report.passed());
    assert!(report.database.is_ok() && report.key_provider.is_ok());
    assert_eq!(report.bucket.unwrap_err().step, BucketStep::Expiry);
}

#[tokio::test]
async fn retire_before_sets_and_clears_the_time_and_counts_active_leases() {
    let rig = Rig::start(Options::default()).await;
    let admin = admin_of(&rig);
    // No router has written the rollout record yet.
    assert!(admin.retire_before(RetireBefore::Now).await.is_err());
    let meta = Meta::new(rig.db.clone());
    let own = BuildSpec {
        build: "0.1.0".into(),
        spec: String::new(),
    };
    knoggin_server::rollout::at_start(&meta, &own, false)
        .await
        .unwrap();
    let a = rig.account().await;
    let c = rig.container(quick()).await;
    rig.lease(a.user, &c, rig.generation()).await;
    assert_eq!(admin.retire_before(RetireBefore::Now).await.unwrap(), 1);
    let record = meta.rollout().await.unwrap().unwrap().value;
    assert_eq!(record.retire_before_ms, Some(rig.clock.now()));
    assert_eq!(record.claim, own);
    let at = Timestamp::parse_rfc3339("2026-11-23T10:00:00Z").unwrap();
    admin.retire_before(RetireBefore::At(at)).await.unwrap();
    assert_eq!(
        meta.rollout()
            .await
            .unwrap()
            .unwrap()
            .value
            .retire_before_ms,
        Some(at.millis())
    );
    admin.retire_before(RetireBefore::Clear).await.unwrap();
    assert_eq!(
        meta.rollout()
            .await
            .unwrap()
            .unwrap()
            .value
            .retire_before_ms,
        None
    );
}

#[tokio::test]
async fn rotating_the_master_key_rewraps_every_key_and_keeps_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("master_key");
    let set = knoggin_crypto::testing::test_key_set();
    let old_kek = set.current_kek_id();
    create_key_file(&file, &set).unwrap();
    let clock = TestClock::new();
    let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
    let provider = knoggin_crypto::LocalKeyProvider::new(set);
    // The field key is made on the empty deployment, as a router's start does.
    let fields = Arc::new(Meta::new(db.clone()).field_keys(&provider).await.unwrap());
    let users = Users::new(db.clone(), fields.clone());
    let stores = Stores::new(db.clone(), fields);
    let user = UserId::from_uuid(uuid::Uuid::now_v7());
    let store = StoreId::from_uuid(uuid::Uuid::now_v7());
    let scope = knoggin_crypto::KeyScope {
        store,
        generation: 0,
    };
    let (old_key, wrapped) = provider.generate(scope).await.unwrap();
    users
        .create(
            NewUser {
                id: user,
                issuer: "https://issuer.test".into(),
                subject: Sensitive::new("s".into()),
                email: None,
                email_verified: false,
                name: None,
            },
            NewStore {
                id: store,
                name: Sensitive::new("Personal".into()),
                wrapped: wrapped.clone(),
            },
            clock.now(),
        )
        .await
        .unwrap();

    let (kek, count) = rotate_master_key(db.clone(), &file).await.unwrap();
    assert_ne!(kek, old_kek);
    assert_eq!(count, 1);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o444
    );
    let rotated = read_key_file(&file, FileRule::NotWritableByOthers).unwrap();
    assert_eq!(rotated.current_kek_id(), kek);
    let after = stores.get_of(user, store).await.unwrap().unwrap();
    assert_eq!(after.value.kek_id, kek);
    let new_provider = knoggin_crypto::LocalKeyProvider::new(rotated);
    let same = new_provider
        .unwrap(scope, &after.value.wrapped().unwrap())
        .await
        .unwrap();
    assert_eq!(same.expose(), old_key.expose(), "the data key itself stays");
    // The old key is still in the set: an old snapshot header still opens.
    assert!(new_provider.unwrap(scope, &wrapped).await.is_ok());
    // The field key opens under the new set, and the records still read.
    let fields = Arc::new(
        Meta::new(db.clone())
            .field_keys(&new_provider)
            .await
            .unwrap(),
    );
    assert!(
        Stores::new(db, fields)
            .get_of(user, store)
            .await
            .unwrap()
            .is_some()
    );
}

/// Restore end to end: the admin command writes the request, the old container's final
/// snapshot does not cancel it, the next container opens the chosen snapshot, saves it
/// at once (with no change), refuses writes until that commit has gone through, and the
/// commit clears `restore_from`. Nothing is deleted.
#[tokio::test]
async fn a_restore_opens_the_chosen_snapshot_and_saves_it_before_any_write() {
    let rig = Rig::start(Options::default()).await;
    let admin = admin_of(&rig);
    let a = rig.account().await;

    // c1 saves "first" and parks: the snapshot to restore.
    let mut c1 = rig.container(quick()).await;
    let g1 = rig.generation();
    rig.lease(a.user, &c1, g1).await;
    assert_eq!(
        c1.claim(a.user, g1, settings(600, GIB)).await,
        StatusCode::OK
    );
    assert_eq!(c1.save(a, "first").await, StatusCode::CREATED);
    assert_eq!(c1.park(ParkReason::Drain).await, StatusCode::ACCEPTED);
    c1.end(SECONDS).await;
    let chosen = SnapshotName::new(g1, 1).unwrap();

    // c2 saves "second" and is still running when the admin asks.
    rig.clock.advance(5_000);
    let mut c2 = rig.container(quick()).await;
    let g2 = rig.generation();
    rig.lease(a.user, &c2, g2).await;
    assert_eq!(
        c2.claim(a.user, g2, settings(600, GIB)).await,
        StatusCode::OK
    );
    assert_eq!(c2.save(a, "second").await, StatusCode::CREATED);

    // The admin accepts an unpadded name; a missing snapshot is refused.
    let missing = knoggin_server::admin::parse_snapshot(&format!("{g1}-9")).unwrap();
    assert!(
        admin
            .restore(a.store, missing, Duration::from_millis(10))
            .await
            .is_err()
    );
    let parsed = knoggin_server::admin::parse_snapshot(&format!("{g1}-1")).unwrap();
    assert_eq!(parsed, chosen);
    let outcome = admin
        .restore(a.store, parsed, Duration::from_millis(50))
        .await
        .unwrap();
    assert_eq!(outcome, RestoreOutcome::Waiting);
    let lease = rig.lease_of(a.user).await;
    assert_eq!(lease.park_requested(), Some(ParkReason::Restore));
    let record = rig.stores.get_of(a.user, a.store).await.unwrap().unwrap();
    assert_eq!(record.value.restore_from, Some(chosen.to_string()));

    // The reconcile loop would send `park`: c2's final snapshot does not cancel it.
    assert_eq!(c2.park(ParkReason::Restore).await, StatusCode::ACCEPTED);
    c2.end(SECONDS).await;
    assert_eq!(rig.lease_of(a.user).await.state, LeaseState::Parked);
    let record = rig.stores.get_of(a.user, a.store).await.unwrap().unwrap();
    assert_eq!(record.value.restore_from, Some(chosen.to_string()));
    assert_eq!(
        record.value.newest.map(|n| (n.generation, n.sequence)),
        Some((g2, 1))
    );
    // With the lease parked the command returns at once.
    assert_eq!(
        admin.restore(a.store, chosen, SECONDS).await.unwrap(),
        RestoreOutcome::Parked
    );

    // c3: its first commits fail, so the restored store stays closed to writes a while.
    rig.fail_commits.store(2, Ordering::SeqCst);
    rig.clock.advance(5_000);
    let mut c3 = rig.container(quick()).await;
    let g3 = rig.generation();
    rig.lease(a.user, &c3, g3).await;
    assert_eq!(
        c3.claim(a.user, g3, settings(600, GIB)).await,
        StatusCode::OK
    );
    let (status, _, body) = c3
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(summaries(&body), ["first"], "the chosen snapshot's content");
    let (status, headers, _) = c3
        .forward(
            a.user,
            a.store,
            Method::POST,
            "/v1/memories",
            r#"{"summary":"too early"}"#,
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(headers.contains_key("retry-after"));

    // Saved at once under c3's generation, though nothing changed; the commit that goes
    // through records the restore.
    let restored = || async {
        rig.stores
            .get_of(a.user, a.store)
            .await
            .unwrap()
            .unwrap()
            .value
            .restore_from
            .is_none()
    };
    assert!(eventually(Duration::from_secs(15), restored).await);
    let record = rig.stores.get_of(a.user, a.store).await.unwrap().unwrap();
    assert_eq!(
        record.value.newest.map(|n| n.generation),
        Some(g3),
        "the restored snapshot was saved under the new generation"
    );
    assert_eq!(rig.fail_commits.load(Ordering::SeqCst), 0);
    // The broker clears `restore_from` before the container has its answer (and after a
    // retried commit the container saves once more under the next sequence), so the
    // container's own block lifts a little later: until then a write is still refused.
    let accepted = || async {
        let status = c3.save(a, "after").await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::SERVICE_UNAVAILABLE,
            "{status}"
        );
        status == StatusCode::CREATED
    };
    assert!(eventually(Duration::from_secs(15), accepted).await);
    let (_, _, body) = c3
        .forward(a.user, a.store, Method::GET, "/v1/memories", Body::empty())
        .await;
    let mut got = summaries(&body);
    got.sort();
    assert_eq!(got, ["after", "first"]);
    c3.stop();
    c3.end(SECONDS).await;

    // Every object is kept: the restore deletes nothing.
    let keys = support::snapshots(&rig, a.store);
    for name in [chosen, SnapshotName::new(g2, 1).unwrap()] {
        assert!(keys.contains(&names::snapshot_key(a.store, name)), "{name}");
    }

    // A cancel clears a request.
    admin.restore(a.store, chosen, SECONDS).await.unwrap();
    admin.cancel_restore(a.store).await.unwrap();
    let record = rig.stores.get_of(a.user, a.store).await.unwrap().unwrap();
    assert_eq!(record.value.restore_from, None);
    // An unknown store is refused.
    assert!(
        admin
            .cancel_restore(StoreId::from_uuid(uuid::Uuid::now_v7()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_admin_snapshot_list_labels_every_object() {
    let rig = Rig::start(Options::default()).await;
    let admin = admin_of(&rig);
    let a = rig.account().await;
    let now = rig.clock.now();
    for seq in 1..=2 {
        rig.storage.insert(
            &names::snapshot_key(a.store, SnapshotName::new(7, seq).unwrap()),
            vec![0u8; 9],
            Timestamp::from_millis(now - 1_000),
        );
    }
    let list = admin.snapshots(a.store, 7).await.unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["sequence"], 2, "newest first");
    assert_eq!(items[0]["keptBecause"], "newest");
    assert_eq!(items[0]["sizeBytes"], 9);
    assert_eq!(items[1]["keptBecause"], serde_json::Value::Null);
}
