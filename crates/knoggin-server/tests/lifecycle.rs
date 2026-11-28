// SPDX-License-Identifier: AGPL-3.0-only
//! The lifecycle task on `MemoryStorage` and the local state file, with a set clock.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;

use knoggin_core::snapshot::SnapshotName;
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_core::{Sensitive, StoreId, Timestamp, UserId};
use knoggin_crypto::KeyProvider;
use knoggin_server::lifecycle::Lifecycle;
use knoggin_server::settings::RetentionSettings;
use knoggin_server::storage::{MemoryStorage, ObjectStorage, names};
use knoggin_state::record::Newest;
use knoggin_state::testing::TestClock;
use knoggin_state::{LocalFileDb, NewStore, NewUser, StateDb, StateError, Stores, Users};
use uuid::Uuid;

const MIN: i64 = 60_000;
const HOUR: i64 = 60 * MIN;
const MARGIN: i64 = 10 * MIN;

struct World {
    clock: TestClock,
    db: Arc<dyn StateDb>,
    users: Users,
    stores: Stores,
    storage: Arc<MemoryStorage>,
    lifecycle: Lifecycle,
}

async fn world() -> World {
    let clock = TestClock::new();
    let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
    let fields = Arc::new(knoggin_state::testing::test_field_keys());
    let storage = Arc::new(MemoryStorage::start(clock.clock()).await.unwrap());
    World {
        users: Users::new(db.clone(), fields.clone()),
        stores: Stores::new(db.clone(), fields),
        lifecycle: Lifecycle {
            db: db.clone(),
            postgres: None,
            storage: storage.clone(),
            retention: RetentionSettings {
                days: 3,
                soft_delete_days: 7,
            },
        },
        clock,
        db,
        storage,
    }
}

impl World {
    async fn user(&self) -> (UserId, StoreId) {
        let user = UserId::from_uuid(Uuid::now_v7());
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = knoggin_crypto::testing::test_provider()
            .generate(store)
            .await
            .unwrap();
        self.users
            .create(
                NewUser {
                    id: user,
                    issuer: "https://issuer.test".into(),
                    subject: Sensitive::new(user.to_string()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                self.clock.now(),
            )
            .await
            .unwrap();
        (user, store)
    }

    async fn app(&self, user: UserId) -> StoreId {
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = knoggin_crypto::testing::test_provider()
            .generate(store)
            .await
            .unwrap();
        self.stores
            .create_app(
                user,
                NewStore {
                    id: store,
                    name: Sensitive::new(format!("app {store}")),
                    wrapped,
                },
                self.clock.now(),
            )
            .await
            .unwrap();
        store
    }

    fn put(&self, store: StoreId, generation: u64, sequence: u64, at: i64) -> String {
        let key = names::snapshot_key(store, SnapshotName::new(generation, sequence).unwrap());
        self.storage
            .insert(&key, vec![1u8; 4], Timestamp::from_millis(at));
        key
    }

    fn keys_of(&self, store: StoreId) -> Vec<String> {
        let prefix = names::store_prefix(store);
        self.storage
            .keys()
            .into_iter()
            .filter(|k| k.starts_with(&prefix))
            .collect()
    }

    async fn run(&self) -> knoggin_server::lifecycle::LifecycleReport {
        self.lifecycle
            .run_once(Timestamp::from_millis(self.clock.now()))
            .await
            .unwrap()
    }

    async fn mark_store(&self, user: UserId, store: StoreId, at: i64) {
        self.stores
            .update(user, store, |s| {
                s.deleted_at_ms = Some(at);
                Ok::<_, StateError>(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn thinning_keeps_the_windows_the_newest_and_the_restore() {
    let w = world().await;
    let (user, store) = w.user().await;
    // A day boundary in the past, so that the windows are easy to see: now is 00:30 UTC.
    let midnight = w.clock.now().div_euclid(MILLIS_PER_DAY) * MILLIS_PER_DAY + MILLIS_PER_DAY;
    w.clock.advance(midnight + 30 * MIN - w.clock.now());
    let now = w.clock.now();
    let g = 1_000;
    // The last hour: two in the 00:20 window (the higher name wins, though it arrived
    // first), one at 00:05.
    let low_late = w.put(store, g, 10, now - 3 * MIN);
    let high_early = w.put(store, g, 11, now - 8 * MIN);
    let early_window = w.put(store, g, 9, now - 25 * MIN);
    // The 23:00 hour of yesterday: two, the higher name kept.
    let hour_low = w.put(store, g, 7, now - 70 * MIN);
    let hour_high = w.put(store, g, 8, now - 80 * MIN);
    // Two days ago: one daily keeper and one that goes.
    let day_keep = w.put(store, g, 5, now - 2 * MILLIS_PER_DAY + HOUR);
    let day_gone = w.put(store, g, 4, now - 2 * MILLIS_PER_DAY);
    // Older than `retention.days`: gone, unless it is the restore.
    let old_gone = w.put(store, g, 2, now - 5 * MILLIS_PER_DAY);
    let restore = w.put(store, g, 1, now - 6 * MILLIS_PER_DAY);
    // Not a snapshot name: left alone.
    let stray = format!("{}stray.txt", names::snapshot_prefix(store));
    w.storage.insert(
        &stray,
        vec![0u8],
        Timestamp::from_millis(now - 9 * MILLIS_PER_DAY),
    );
    // The recorded newest is below junk that was never committed; both stay.
    let junk = w.put(store, g + 5, 1, now - 9 * MILLIS_PER_DAY);
    w.stores
        .update(user, store, |s| {
            s.newest = Some(Newest {
                generation: g,
                sequence: 3,
            });
            s.restore_from = Some(SnapshotName::new(g, 1).unwrap().to_string());
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let newest = w.put(store, g, 3, now - 4 * MILLIS_PER_DAY);

    let report = w.run().await;
    assert_eq!(report.stores_thinned, 1);
    let left = w.keys_of(store);
    for kept in [
        &high_early,
        &early_window,
        &hour_high,
        &day_keep,
        &restore,
        &newest,
        &junk,
        &stray,
    ] {
        assert!(left.contains(kept), "{kept} was deleted");
    }
    for gone in [&low_late, &hour_low, &day_gone, &old_gone] {
        assert!(!left.contains(gone), "{gone} was kept");
    }
    assert_eq!(report.snapshots_deleted, 4);
    // A second run deletes nothing more.
    assert_eq!(w.run().await.snapshots_deleted, 0);
}

#[tokio::test]
async fn deleted_stores_and_users_are_erased_after_the_undo_period_and_ten_minutes() {
    let w = world().await;
    let days = 7 * MILLIS_PER_DAY;
    // A deleted app of a live user; the personal store stays.
    let (alice, personal) = w.user().await;
    let app = w.app(alice).await;
    w.put(app, 5, 1, w.clock.now());
    w.put(personal, 5, 1, w.clock.now());
    w.mark_store(alice, app, w.clock.now()).await;
    // A deleted user whose personal store never got a mark of its own.
    let (bob, bob_store) = w.user().await;
    w.put(bob_store, 5, 1, w.clock.now());
    w.users
        .update(bob, |u| {
            u.deleted_at_ms = Some(w.clock.now());
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    // An old self-test object goes; a new one stays.
    let old_test = format!("{}old", names::SELFTEST);
    w.storage
        .insert(&old_test, vec![0u8], Timestamp::from_millis(w.clock.now()));

    // Just past the undo period, but inside the 10 minutes: nothing is erased.
    w.clock.advance(days + MARGIN - MIN);
    let new_test = format!("{}new", names::SELFTEST);
    w.storage
        .insert(&new_test, vec![0u8], Timestamp::from_millis(w.clock.now()));
    let r = w.run().await;
    assert_eq!((r.stores_erased, r.users_erased), (0, 0));
    assert_eq!(r.selftest_deleted, 1);
    assert_eq!(
        w.storage
            .keys()
            .iter()
            .filter(|k| k.starts_with("selftest/"))
            .count(),
        1
    );
    assert!(w.stores.get(app).await.unwrap().is_some());

    // After the 10 minutes: the app and Bob are gone, objects and records alike.
    w.clock.advance(2 * MIN);
    let r = w.run().await;
    assert_eq!(
        (r.stores_erased, r.users_erased, r.stores_failed),
        (2, 1, 0)
    );
    assert!(w.keys_of(app).is_empty());
    assert!(w.keys_of(bob_store).is_empty());
    assert!(w.stores.get(app).await.unwrap().is_none());
    assert!(w.stores.get(bob_store).await.unwrap().is_none());
    assert!(w.users.get(bob).await.unwrap().is_none());
    // Alice and her personal store are untouched.
    assert!(w.users.get(alice).await.unwrap().is_some());
    assert_eq!(w.keys_of(personal).len(), 1);
    assert!(w.stores.get(personal).await.unwrap().is_some());
}

#[tokio::test]
async fn an_erase_that_stopped_after_the_objects_is_finished_by_the_next_run() {
    let w = world().await;
    let (alice, _) = w.user().await;
    let app = w.app(alice).await;
    w.put(app, 5, 1, w.clock.now());
    w.mark_store(alice, app, w.clock.now()).await;
    w.clock.advance(7 * MILLIS_PER_DAY + MARGIN + MIN);
    // An earlier run deleted the objects and then stopped.
    for k in w.keys_of(app) {
        w.storage.remove(&k);
    }
    assert!(w.stores.get(app).await.unwrap().is_some());
    let r = w.run().await;
    assert_eq!(r.stores_erased, 1);
    assert!(w.stores.get(app).await.unwrap().is_none());
}

#[tokio::test]
async fn a_bucket_that_fails_erases_no_record() {
    let w = world().await;
    let (alice, _) = w.user().await;
    let app = w.app(alice).await;
    w.put(app, 5, 1, w.clock.now());
    w.mark_store(alice, app, w.clock.now()).await;
    w.clock.advance(7 * MILLIS_PER_DAY + MARGIN + MIN);
    w.storage
        .set_fault(knoggin_server::storage::Fault::Fail)
        .await
        .unwrap();
    assert!(
        w.lifecycle
            .run_once(Timestamp::from_millis(w.clock.now()))
            .await
            .is_err()
    );
    assert!(w.stores.get(app).await.unwrap().is_some());
    w.storage
        .set_fault(knoggin_server::storage::Fault::None)
        .await
        .unwrap();
    assert_eq!(w.run().await.stores_erased, 1);
    assert!(
        w.storage
            .list(&names::store_prefix(app))
            .await
            .unwrap()
            .is_empty()
    );
}
