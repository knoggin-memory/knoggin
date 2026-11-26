// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers and the conformance suite that every backend passes:
//! `conformance(db, &clock)` covers the interface (conditions, all or nothing, the write
//! limit, queries with prefix, limit and `after`, expiry, concurrent writers) and every
//! typed helper's rules. A backend's test builds an empty database whose clock is
//! `TestClock::clock()` and calls it; Postgres and DynamoDB run the same function.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::{
    KeyProvider, LocalKeyProvider, testing::test_key_set, testing::test_provider,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::db::{Clock, Cond, Item, MAX_WRITES, StateDb, StateError, Write};
use crate::keys::{self, ONE};
use crate::record::{
    Container, ContainerState, EndKind, Leader, Lease, LeaseState, ParkReason, Permission, Rollout,
    StopReason, StoreKind, StoreRecord, Versioned, utc_day,
};
use crate::seal::FieldKeys;
use crate::stores::{MAX_APPS, store_writes};
use crate::{
    ApiKeys, Containers, Leases, Meta, NewApiKey, NewStore, NewUser, Sessions, Stores, Users,
    erase, rewrap_master_key,
};

/// Where the suite starts its clock: 2027-01-15.
pub const START_MS: i64 = 1_800_000_000_000;

/// A clock that only moves when a test moves it.
#[derive(Debug, Clone, Default)]
pub struct TestClock(Arc<AtomicI64>);

impl TestClock {
    /// A clock at `START_MS`.
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(START_MS)))
    }
    /// The `Clock` to give a backend.
    pub fn clock(&self) -> Clock {
        let t = self.0.clone();
        Arc::new(move || t.load(Ordering::SeqCst))
    }
    /// The time now.
    pub fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
    /// Moves the clock forward.
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// Field keys over a fixed field key.
pub fn test_field_keys() -> FieldKeys {
    FieldKeys::new(&knoggin_crypto::DataKey::from_bytes([0x42; 32]))
}

/// Runs every case on `db`, which must start empty and read its time from `clock`.
pub async fn conformance(db: Arc<dyn StateDb>, clock: &TestClock) {
    // Meta first: the field key may be made only while no user exists.
    let keys = Arc::new(meta_cases(&db).await);
    puts_and_conditions(&*db).await;
    deletes(&*db).await;
    checks(&*db).await;
    all_or_nothing(&*db).await;
    write_limits(&*db).await;
    queries(&*db).await;
    expiry(&*db, clock).await;
    concurrent_writers(&db).await;
    users(&db, &keys, clock).await;
    field_key_missing(&db).await;
    stores(&db, &keys, clock).await;
    api_keys(&db, &keys, clock).await;
    sessions(&db, &keys, clock).await;
    leases_and_containers(&db, clock).await;
    leader_and_rollout(&db, clock).await;
    erase_cases(&db, &keys, clock).await;
    rewrap(&db, &keys).await;
}

// ---- small helpers ----

fn item(pk: &str, sk: &str, n: i64) -> Item {
    Item::new(pk, sk, json!({ "n": n }))
}

async fn put(db: &dyn StateDb, pk: &str, sk: &str, cond: Cond) -> Result<(), StateError> {
    db.write(vec![Write::put(item(pk, sk, 0), cond)]).await
}

async fn version(db: &dyn StateDb, pk: &str, sk: &str) -> Option<u64> {
    db.get(pk, sk).await.unwrap().map(|i| i.version)
}

#[track_caller]
fn conflict(r: Result<(), StateError>) {
    assert!(
        matches!(r, Err(StateError::Conflict)),
        "expected Conflict, got {r:?}"
    );
}

#[track_caller]
fn invalid(r: Result<(), StateError>) {
    assert!(
        matches!(r, Err(StateError::Invalid(_))),
        "expected Invalid, got {r:?}"
    );
}

fn sks(items: &[Item]) -> Vec<&str> {
    items.iter().map(|i| i.sk.as_str()).collect()
}

fn s(text: &str) -> Sensitive<String> {
    Sensitive::new(text.to_owned())
}

fn uid() -> UserId {
    UserId::from_uuid(Uuid::now_v7())
}

fn sid() -> StoreId {
    StoreId::from_uuid(Uuid::now_v7())
}

async fn raw(db: &dyn StateDb, pk: &str, sk: &str) -> Value {
    db.get(pk, sk).await.unwrap().expect("item exists").doc
}

async fn new_store(name: &str) -> NewStore {
    let id = sid();
    let (_, wrapped) = test_provider().generate(id).await.unwrap();
    NewStore {
        id,
        name: s(name),
        wrapped,
    }
}

async fn make_user(
    users: &Users,
    issuer: &str,
    subject: &str,
    now: i64,
) -> Versioned<crate::record::User> {
    let new = NewUser {
        id: uid(),
        issuer: issuer.into(),
        subject: s(subject),
        email: Some(s(&format!("marker-email-{subject}@example.com"))),
        email_verified: true,
        name: Some(s(&format!("marker-name-{subject}"))),
    };
    let (u, created) = users
        .create(new, new_store("Personal").await, now)
        .await
        .unwrap();
    assert!(created);
    u
}

// ---- the interface ----

async fn meta_cases(db: &Arc<dyn StateDb>) -> FieldKeys {
    let meta = Meta::new(db.clone());
    meta.check_schema().await.unwrap();
    meta.check_schema().await.unwrap();
    assert_eq!(raw(&**db, "meta", "schema").await, json!({ "version": 1 }));
    db.write(vec![Write::put(
        Item::new("meta", "schema", json!({ "version": 2 })),
        Cond::Any,
    )])
    .await
    .unwrap();
    assert!(matches!(
        meta.check_schema().await,
        Err(StateError::SchemaTooNew(2))
    ));
    db.write(vec![Write::put(
        Item::new("meta", "schema", json!({ "version": 1 })),
        Cond::Any,
    )])
    .await
    .unwrap();

    let provider = test_provider();
    let first = meta.field_keys(&provider).await.unwrap();
    let again = meta.field_keys(&provider).await.unwrap();
    let sealed = first.seal_field("p", "s", "f", "value").unwrap();
    assert_eq!(again.open_field("p", "s", "f", &sealed).unwrap(), "value");
    let rec = raw(&**db, "meta", "field-key").await;
    assert_eq!(rec["kek_id"], json!(test_key_set().current_kek_id()));
    first
}

/// After users exist, a missing field key record refuses to start.
async fn field_key_missing(db: &Arc<dyn StateDb>) {
    let meta = Meta::new(db.clone());
    let saved = db.get("meta", "field-key").await.unwrap().unwrap();
    db.write(vec![Write::delete("meta", "field-key", Cond::Any)])
        .await
        .unwrap();
    assert!(matches!(
        meta.field_keys(&test_provider()).await,
        Err(StateError::FieldKeyMissing)
    ));
    db.write(vec![Write::put(saved, Cond::Absent)])
        .await
        .unwrap();
    meta.field_keys(&test_provider()).await.unwrap();
}

async fn puts_and_conditions(db: &dyn StateDb) {
    let pk = "c-put";
    assert!(db.get(pk, "a").await.unwrap().is_none());
    // Any: a new item is version 1, each put adds 1; a version the caller set is ignored.
    let mut it = item(pk, "a", 1);
    it.version = 99;
    db.write(vec![Write::put(it, Cond::Any)]).await.unwrap();
    assert_eq!(version(db, pk, "a").await, Some(1));
    put(db, pk, "a", Cond::Any).await.unwrap();
    assert_eq!(version(db, pk, "a").await, Some(2));
    // The document and the expiry round-trip.
    let doc =
        json!({ "s": "Grüße ✓ \"q\"", "n": -12, "f": 1.5, "a": [1, null, {"x": true}], "z": null });
    let far = START_MS + 1_000 * MILLIS_PER_DAY;
    db.write(vec![Write::put(
        Item::new(pk, "doc", doc.clone()).expiring(Some(far)),
        Cond::Any,
    )])
    .await
    .unwrap();
    let got = db.get(pk, "doc").await.unwrap().unwrap();
    assert_eq!((got.pk.as_str(), got.sk.as_str()), (pk, "doc"));
    assert_eq!(got.doc, doc);
    assert_eq!(got.expires_ms, Some(far));
    // A put without an expiry removes it.
    db.write(vec![Write::put(Item::new(pk, "doc", doc), Cond::Any)])
        .await
        .unwrap();
    assert_eq!(db.get(pk, "doc").await.unwrap().unwrap().expires_ms, None);
    // Absent.
    put(db, pk, "b", Cond::Absent).await.unwrap();
    assert_eq!(version(db, pk, "b").await, Some(1));
    conflict(put(db, pk, "b", Cond::Absent).await);
    assert_eq!(version(db, pk, "b").await, Some(1));
    // Version(n).
    put(db, pk, "b", Cond::Version(1)).await.unwrap();
    assert_eq!(version(db, pk, "b").await, Some(2));
    conflict(put(db, pk, "b", Cond::Version(1)).await);
    conflict(put(db, pk, "b", Cond::Version(3)).await);
    conflict(put(db, pk, "missing", Cond::Version(1)).await);
    assert!(db.get(pk, "missing").await.unwrap().is_none());
    assert_eq!(version(db, pk, "b").await, Some(2));
}

async fn deletes(db: &dyn StateDb) {
    let pk = "c-del";
    put(db, pk, "a", Cond::Any).await.unwrap();
    db.write(vec![Write::delete(pk, "a", Cond::Any)])
        .await
        .unwrap();
    assert!(db.get(pk, "a").await.unwrap().is_none());
    db.write(vec![Write::delete(pk, "a", Cond::Any)])
        .await
        .unwrap();
    put(db, pk, "b", Cond::Any).await.unwrap();
    conflict(
        db.write(vec![Write::delete(pk, "b", Cond::Version(2))])
            .await,
    );
    assert_eq!(version(db, pk, "b").await, Some(1));
    db.write(vec![Write::delete(pk, "b", Cond::Version(1))])
        .await
        .unwrap();
    assert!(db.get(pk, "b").await.unwrap().is_none());
    conflict(
        db.write(vec![Write::delete(pk, "b", Cond::Version(1))])
            .await,
    );
    invalid(db.write(vec![Write::delete(pk, "b", Cond::Absent)]).await);
    // A deleted item starts again at version 1.
    put(db, pk, "b", Cond::Absent).await.unwrap();
    assert_eq!(version(db, pk, "b").await, Some(1));
}

async fn checks(db: &dyn StateDb) {
    let pk = "c-chk";
    put(db, pk, "a", Cond::Any).await.unwrap();
    db.write(vec![
        Write::check(pk, "a", Cond::Version(1)),
        Write::put(item(pk, "b", 0), Cond::Any),
    ])
    .await
    .unwrap();
    assert_eq!(
        version(db, pk, "a").await,
        Some(1),
        "a check changes nothing"
    );
    assert!(db.get(pk, "b").await.unwrap().is_some());
    conflict(
        db.write(vec![
            Write::check(pk, "a", Cond::Version(9)),
            Write::put(item(pk, "c", 0), Cond::Any),
        ])
        .await,
    );
    assert!(db.get(pk, "c").await.unwrap().is_none());
    db.write(vec![
        Write::check(pk, "nothing", Cond::Absent),
        Write::put(item(pk, "d", 0), Cond::Any),
    ])
    .await
    .unwrap();
    conflict(
        db.write(vec![
            Write::check(pk, "a", Cond::Absent),
            Write::put(item(pk, "e", 0), Cond::Any),
        ])
        .await,
    );
    assert!(db.get(pk, "e").await.unwrap().is_none());
    conflict(
        db.write(vec![
            Write::check(pk, "nothing", Cond::Version(1)),
            Write::put(item(pk, "e", 0), Cond::Any),
        ])
        .await,
    );
    invalid(
        db.write(vec![
            Write::check(pk, "a", Cond::Any),
            Write::put(item(pk, "e", 0), Cond::Any),
        ])
        .await,
    );
    assert!(db.get(pk, "e").await.unwrap().is_none());
}

async fn all_or_nothing(db: &dyn StateDb) {
    let pk = "c-aon";
    put(db, pk, "x1", Cond::Any).await.unwrap();
    put(db, pk, "x2", Cond::Any).await.unwrap();
    // The failing condition comes last, then first: neither applies anything.
    for bad_first in [false, true] {
        let mut writes = vec![
            Write::put(item(pk, "x1", 7), Cond::Version(1)),
            Write::delete(pk, "x2", Cond::Any),
            Write::put(item(pk, "x3", 7), Cond::Absent),
        ];
        let bad = Write::put(item(pk, "x4", 7), Cond::Version(5));
        if bad_first {
            writes.insert(0, bad);
        } else {
            writes.push(bad);
        }
        conflict(db.write(writes).await);
        let x1 = db.get(pk, "x1").await.unwrap().unwrap();
        assert_eq!((x1.version, &x1.doc), (1, &json!({ "n": 0 })));
        assert!(db.get(pk, "x2").await.unwrap().is_some());
        assert!(db.get(pk, "x3").await.unwrap().is_none());
        assert!(db.get(pk, "x4").await.unwrap().is_none());
    }
    // The same writes with the condition fixed all apply.
    db.write(vec![
        Write::put(item(pk, "x1", 7), Cond::Version(1)),
        Write::delete(pk, "x2", Cond::Any),
        Write::put(item(pk, "x3", 7), Cond::Absent),
    ])
    .await
    .unwrap();
    assert_eq!(version(db, pk, "x1").await, Some(2));
    assert!(db.get(pk, "x2").await.unwrap().is_none());
    assert!(db.get(pk, "x3").await.unwrap().is_some());
}

async fn write_limits(db: &dyn StateDb) {
    let full: Vec<Write> = (0..MAX_WRITES)
        .map(|i| Write::put(item("c-lim25", &format!("{i:02}"), 0), Cond::Absent))
        .collect();
    db.write(full).await.unwrap();
    assert_eq!(db.query("c-lim25", "", 100, None).await.unwrap().len(), 25);
    let over: Vec<Write> = (0..=MAX_WRITES)
        .map(|i| Write::put(item("c-lim26", &format!("{i:02}"), 0), Cond::Any))
        .collect();
    invalid(db.write(over).await);
    assert!(db.query("c-lim26", "", 100, None).await.unwrap().is_empty());
    invalid(
        db.write(vec![
            Write::put(item("c-lim", "a", 0), Cond::Any),
            Write::check("c-lim", "a", Cond::Absent),
        ])
        .await,
    );
    invalid(
        db.write(vec![Write::put(item("c-lim", "", 0), Cond::Any)])
            .await,
    );
    invalid(
        db.write(vec![Write::put(item("", "a", 0), Cond::Any)])
            .await,
    );
    assert!(db.get("c-lim", "a").await.unwrap().is_none());
    db.write(Vec::new()).await.unwrap();
}

async fn queries(db: &dyn StateDb) {
    let pk = "c-q";
    // Byte order: "A" < "a" < "a#1" < "a#2" < "ab" < "b" < "b#1" < "é" (0xC3).
    let order = ["A", "a", "a#1", "a#2", "ab", "b", "b#1", "\u{e9}"];
    let mut writes: Vec<Write> = order
        .iter()
        .rev()
        .map(|k| Write::put(item(pk, k, 0), Cond::Absent))
        .collect();
    writes.push(Write::put(item("c-q2", "a", 0), Cond::Absent));
    writes.push(Write::put(item("c-q#", "a", 0), Cond::Absent));
    writes.push(Write::put(item("c-", "a", 0), Cond::Absent));
    db.write(writes).await.unwrap();

    let q = |prefix: &'static str, limit: usize, after: Option<&'static str>| async move {
        db.query(pk, prefix, limit, after).await.unwrap()
    };
    assert_eq!(sks(&q("", 100, None).await), order);
    assert_eq!(sks(&q("a", 100, None).await), ["a", "a#1", "a#2", "ab"]);
    assert_eq!(sks(&q("a#", 100, None).await), ["a#1", "a#2"]);
    assert_eq!(sks(&q("a", 2, None).await), ["a", "a#1"]);
    assert_eq!(sks(&q("a", 2, Some("a#1")).await), ["a#2", "ab"]);
    assert_eq!(sks(&q("a", 10, Some("a#15")).await), ["a#2", "ab"]);
    assert_eq!(sks(&q("", 2, Some("ab")).await), ["b", "b#1"]);
    assert_eq!(sks(&q("b", 10, Some("a")).await), ["b", "b#1"]);
    assert!(q("a", 10, Some("ab")).await.is_empty());
    assert!(q("a", 0, None).await.is_empty());
    assert!(q("c", 10, None).await.is_empty());
    assert!(db.query("c-none", "", 10, None).await.unwrap().is_empty());
    assert_eq!(sks(&db.query("c-q2", "", 10, None).await.unwrap()), ["a"]);
    // Paging collects the whole partition in order, ending on a short page.
    let mut all = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = db.query(pk, "", 3, after.as_deref()).await.unwrap();
        let short = page.len() < 3;
        after = page.last().map(|i| i.sk.clone());
        all.extend(page);
        if short {
            break;
        }
    }
    assert_eq!(sks(&all), order);
}

async fn expiry(db: &dyn StateDb, clock: &TestClock) {
    let pk = "c-exp";
    let now = clock.now();
    db.write(vec![Write::put(
        item(pk, "e", 0).expiring(Some(now + 1000)),
        Cond::Absent,
    )])
    .await
    .unwrap();
    db.write(vec![Write::put(item(pk, "live", 0), Cond::Absent)])
        .await
        .unwrap();
    clock.advance(999);
    assert!(
        db.get(pk, "e").await.unwrap().is_some(),
        "live while expires_ms > now"
    );
    clock.advance(1);
    assert!(
        db.get(pk, "e").await.unwrap().is_none(),
        "gone at expires_ms == now"
    );
    assert_eq!(sks(&db.query(pk, "", 10, None).await.unwrap()), ["live"]);
    conflict(put(db, pk, "e", Cond::Version(1)).await);
    conflict(
        db.write(vec![Write::delete(pk, "e", Cond::Version(1))])
            .await,
    );
    conflict(
        db.write(vec![
            Write::check(pk, "e", Cond::Version(1)),
            Write::put(item(pk, "x", 0), Cond::Any),
        ])
        .await,
    );
    db.write(vec![
        Write::check(pk, "e", Cond::Absent),
        Write::put(item(pk, "x", 0), Cond::Any),
    ])
    .await
    .unwrap();
    put(db, pk, "e", Cond::Absent).await.unwrap();
    assert_eq!(
        version(db, pk, "e").await,
        Some(1),
        "a put over an expired item starts at 1"
    );
    // Any over an expired item: 1 or one more than before, by backend.
    let now = clock.now();
    db.write(vec![Write::put(item(pk, "f", 0), Cond::Any)])
        .await
        .unwrap();
    db.write(vec![Write::put(
        item(pk, "f", 0).expiring(Some(now + 1)),
        Cond::Any,
    )])
    .await
    .unwrap();
    clock.advance(1);
    put(db, pk, "f", Cond::Any).await.unwrap();
    let v = version(db, pk, "f").await.unwrap();
    assert!(v == 1 || v == 3, "version {v}");
    // An item written already expired is not there.
    db.write(vec![Write::put(
        item(pk, "past", 0).expiring(Some(now - 5)),
        Cond::Any,
    )])
    .await
    .unwrap();
    assert!(db.get(pk, "past").await.unwrap().is_none());
    // A query keeps going past expired items until it holds `limit` live ones.
    let now = clock.now();
    let writes = (0..10)
        .map(|i| {
            let exp = (i % 2 == 0).then_some(now + 10);
            Write::put(
                item("c-exp-q", &format!("k{i}"), i).expiring(exp),
                Cond::Absent,
            )
        })
        .collect();
    db.write(writes).await.unwrap();
    clock.advance(10);
    assert_eq!(
        sks(&db.query("c-exp-q", "", 3, None).await.unwrap()),
        ["k1", "k3", "k5"]
    );
    assert_eq!(
        sks(&db.query("c-exp-q", "k", 3, Some("k5")).await.unwrap()),
        ["k7", "k9"]
    );
}

async fn concurrent_writers(db: &Arc<dyn StateDb>) {
    let pk = "c-race";
    put(&**db, pk, "one", Cond::Any).await.unwrap();
    for round in 0..5 {
        let v = version(&**db, pk, "one").await.unwrap();
        let tasks: Vec<_> = (0..50)
            .map(|n| {
                let db = db.clone();
                tokio::spawn(async move {
                    db.write(vec![Write::put(item(pk, "one", n), Cond::Version(v))])
                        .await
                })
            })
            .collect();
        let mut won = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(()) => won += 1,
                Err(StateError::Conflict) => {}
                Err(e) => panic!("round {round}: {e:?}"),
            }
        }
        assert_eq!(won, 1, "round {round}: exactly one writer wins");
        assert_eq!(version(&**db, pk, "one").await, Some(v + 1));
    }
}

// ---- typed helpers ----

async fn users(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>, clock: &TestClock) {
    let users = Users::new(db.clone(), keys.clone());
    let now = clock.now();
    let issuer = "https://issuer.example";
    let alice = make_user(&users, issuer, "marker-sub-alice", now).await;
    let id = alice.value.id;
    let store = alice.value.personal_store_id;

    // Six items, the personal store among them.
    let doc = raw(&**db, &keys::user(id), ONE).await;
    let text = doc.to_string();
    assert!(
        !text.contains("marker"),
        "no personal field in the clear: {text}"
    );
    for f in ["subject", "email", "name"] {
        assert!(doc[f].as_str().unwrap().starts_with("v1."), "{f} is sealed");
    }
    assert_eq!(doc["issuer"], json!(issuer));
    assert_eq!(doc["deleted_by"], Value::Null);
    let sub_hash = doc["sub_hash"].as_str().unwrap().to_owned();
    assert_eq!(sub_hash.len(), 64);
    assert_eq!(
        raw(&**db, &keys::sub(&sub_hash), ONE).await,
        json!({ "user_id": id })
    );
    assert_eq!(raw(&**db, keys::USERS, &id.to_string()).await, json!({}));
    assert_eq!(
        raw(&**db, &keys::store(store), ONE).await,
        json!({ "user_id": id })
    );
    let sdoc = raw(&**db, &keys::user_stores(id), &store.to_string()).await;
    assert_eq!(sdoc["kind"], json!("personal"));
    assert!(sdoc["name"].as_str().unwrap().starts_with("v1."));
    let names = db
        .query(&keys::user_store_names(id), "", 10, None)
        .await
        .unwrap();
    assert_eq!(names.len(), 1);
    assert_eq!(names[0].doc, json!({ "store_id": store }));

    let found = users
        .find(issuer, "marker-sub-alice")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.value, alice.value);
    assert_eq!(
        found.value.email.as_ref().unwrap().expose(),
        "marker-email-marker-sub-alice@example.com"
    );
    assert!(users.find(issuer, "someone-else").await.unwrap().is_none());
    assert!(
        users
            .find("https://other", "marker-sub-alice")
            .await
            .unwrap()
            .is_none()
    );

    // The same person again: the existing user, nothing new written.
    let again = NewUser {
        id: uid(),
        issuer: issuer.into(),
        subject: s("marker-sub-alice"),
        email: None,
        email_verified: false,
        name: None,
    };
    let other = new_store("Personal").await;
    let other_id = other.id;
    let (got, created) = users.create(again.clone(), other, now).await.unwrap();
    assert!(!created);
    assert_eq!(got.value.id, id);
    assert!(db.get(&keys::user(again.id), ONE).await.unwrap().is_none());
    assert!(db.get(&keys::store(other_id), ONE).await.unwrap().is_none());

    // Two first requests at once create one user.
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let users = users.clone();
        let store = new_store("Personal").await;
        tasks.push(tokio::spawn(async move {
            let new = NewUser {
                id: uid(),
                issuer: issuer.into(),
                subject: s("marker-sub-bob"),
                email: None,
                email_verified: false,
                name: None,
            };
            users.create(new, store, now).await.unwrap()
        }));
    }
    let a = tasks.remove(0).await.unwrap();
    let b = tasks.remove(0).await.unwrap();
    assert_eq!(a.0.value.id, b.0.value.id);
    assert!(a.1 ^ b.1, "exactly one created");

    // Bookkeeping: only when stale, keeps sealed strings, never undoes a newer write.
    let before = raw(&**db, &keys::user(id), ONE).await;
    users.touch_active(&alice, now).await.unwrap();
    let after = db.get(&keys::user(id), ONE).await.unwrap().unwrap();
    assert_eq!(after.version, alice.version + 1);
    assert_eq!(after.doc["last_active_day"], json!(utc_day(now)));
    for f in ["subject", "email", "name"] {
        assert_eq!(after.doc[f], before[f], "{f} keeps its sealed string");
    }
    let fresh = users.get(id).await.unwrap().unwrap();
    users.touch_active(&fresh, now + 1000).await.unwrap();
    assert_eq!(
        version(&**db, &keys::user(id), ONE).await,
        Some(fresh.version)
    );
    // A stale read conflicts and is ignored.
    users
        .touch_active(&alice, now + MILLIS_PER_DAY)
        .await
        .unwrap();
    let cur = users.get(id).await.unwrap().unwrap();
    assert_eq!(cur.version, fresh.version);
    assert_eq!(cur.value.last_active_day, Some(utc_day(now)));

    // Update: a changed email is sealed again, an unchanged name keeps its string.
    let (_, updated) = users
        .update(id, |u| {
            u.email = Some(s("marker-new@example.com"));
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let doc = raw(&**db, &keys::user(id), ONE).await;
    assert_ne!(doc["email"], before["email"]);
    assert_eq!(doc["name"], before["name"]);
    assert_eq!(
        updated.value.email.unwrap().expose(),
        "marker-new@example.com"
    );
    assert_eq!(
        users
            .get(id)
            .await
            .unwrap()
            .unwrap()
            .value
            .email
            .unwrap()
            .expose(),
        "marker-new@example.com"
    );
    let r = users
        .update(id, |u| {
            u.issuer = "https://evil".into();
            Ok::<_, StateError>(())
        })
        .await;
    assert!(matches!(r, Err(StateError::Invalid(_))));
    let r = users.update(uid(), |_| Ok::<_, StateError>(())).await;
    assert!(matches!(r, Err(StateError::NotFound)));
    // A refusal from the closure ends the loop and writes nothing.
    let v = version(&**db, &keys::user(id), ONE).await;
    let r = users
        .update(id, |_| Err::<(), _>(StateError::StoreDeleted))
        .await;
    assert!(matches!(r, Err(StateError::StoreDeleted)));
    assert_eq!(version(&**db, &keys::user(id), ONE).await, v);

    let listed = users.list(1000, None).await.unwrap();
    assert!(listed.iter().any(|u| u.value.id == id));
    assert!(listed.iter().any(|u| u.value.id == a.0.value.id));
    let first = users.list(1, None).await.unwrap();
    let rest = users.list(1000, Some(first[0].value.id)).await.unwrap();
    assert_eq!(first.len() + rest.len(), listed.len());
}

async fn stores(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>, clock: &TestClock) {
    let users = Users::new(db.clone(), keys.clone());
    let stores = Stores::new(db.clone(), keys.clone());
    let now = clock.now();
    let carol = make_user(&users, "https://issuer.example", "carol", now)
        .await
        .value;

    let notes = new_store("marker-app-Notes").await;
    let nid = notes.id;
    let created = stores.create_app(carol.id, notes, now).await.unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(created.value.kind, StoreKind::App);
    let by_id = stores.get(nid).await.unwrap().unwrap();
    assert_eq!(by_id.value, created.value);
    assert_eq!(by_id.value.name.expose(), "marker-app-Notes");
    assert!(
        !raw(&**db, &keys::user_stores(carol.id), &nid.to_string())
            .await
            .to_string()
            .contains("marker")
    );
    for dup in ["marker-app-notes", "MARKER-APP-NOTES"] {
        let r = stores.create_app(carol.id, new_store(dup).await, now).await;
        assert!(matches!(r, Err(StateError::AlreadyExists)), "{dup}: {r:?}");
    }
    let list = stores.list(carol.id).await.unwrap();
    assert_eq!(list.len(), 2);
    assert!(stores.get(sid()).await.unwrap().is_none());

    // Updates keep the sealed name and move the version.
    let name_before =
        raw(&**db, &keys::user_stores(carol.id), &nid.to_string()).await["name"].clone();
    let (_, up) = stores
        .update(carol.id, nid, |r| {
            r.memory_count = 5;
            r.last_write_client = Some(s("marker-client"));
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    assert_eq!(up.version, 2);
    let doc = raw(&**db, &keys::user_stores(carol.id), &nid.to_string()).await;
    assert_eq!(doc["name"], name_before);
    assert!(
        doc["last_write_client"]
            .as_str()
            .unwrap()
            .starts_with("v1.")
    );
    assert_eq!(
        stores
            .get_of(carol.id, nid)
            .await
            .unwrap()
            .unwrap()
            .value
            .last_write_client
            .unwrap()
            .expose(),
        "marker-client"
    );
    let r = stores
        .update(carol.id, nid, |r| {
            r.name = s("other");
            Ok::<_, StateError>(())
        })
        .await;
    assert!(matches!(r, Err(StateError::Invalid(_))));
    // Concurrent writers to one store record all land, through the retry.
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let stores = stores.clone();
            let user = carol.id;
            tokio::spawn(async move {
                stores
                    .update(user, nid, |r| {
                        r.namespace_count += 1;
                        Ok::<_, StateError>(())
                    })
                    .await
                    .map(|_| ())
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    assert_eq!(
        stores
            .get_of(carol.id, nid)
            .await
            .unwrap()
            .unwrap()
            .value
            .namespace_count,
        3
    );

    // Rename moves the name item; a taken name is refused; a change of case keeps the item.
    let names_pk = keys::user_store_names(carol.id);
    stores.rename(carol.id, nid, s("Journal")).await.unwrap();
    let names = db.query(&names_pk, "", 10, None).await.unwrap();
    assert_eq!(names.len(), 2);
    assert!(
        names
            .iter()
            .any(|n| n.sk == keys::name_hash(keys, "journal"))
    );
    assert!(
        !names
            .iter()
            .any(|n| n.sk == keys::name_hash(keys, "marker-app-notes"))
    );
    let r = stores.rename(carol.id, nid, s("PERSONAL")).await;
    assert!(matches!(r, Err(StateError::AlreadyExists)), "{r:?}");
    let renamed = stores.rename(carol.id, nid, s("JOURNAL")).await.unwrap();
    assert_eq!(renamed.value.name.expose(), "JOURNAL");
    assert_eq!(db.query(&names_pk, "", 10, None).await.unwrap().len(), 2);
    stores
        .create_app(carol.id, new_store("marker-app-Notes").await, now)
        .await
        .unwrap();

    // At most 1,000 apps, counting deleted ones; the personal store does not count.
    let dave = make_user(&users, "https://issuer.example", "dave", now)
        .await
        .value;
    let mut seed = Vec::new();
    for i in 0..MAX_APPS - 1 {
        let new = new_store(&format!("app {i}")).await;
        let mut rec = StoreRecord::new(
            new.id,
            dave.id,
            StoreKind::App,
            format!("app {i}"),
            &new.wrapped,
            now,
        );
        if i == 0 {
            // Deleted apps count too. A mark far ahead keeps the erase cases from erasing it.
            rec.deleted_at_ms = Some(now + 1000 * MILLIS_PER_DAY);
        }
        seed.push(store_writes(keys, &rec).unwrap().remove(0));
    }
    for chunk in seed.chunks(MAX_WRITES) {
        db.write(chunk.to_vec()).await.unwrap();
    }
    stores
        .create_app(dave.id, new_store("one more").await, now)
        .await
        .unwrap();
    let r = stores
        .create_app(dave.id, new_store("too many").await, now)
        .await;
    assert!(matches!(r, Err(StateError::LimitReached)), "{r:?}");
    // Remove the seeded records again, so that the later cases stay small.
    let wipe: Vec<Write> = seed
        .iter()
        .map(|w| {
            let (pk, sk) = w.key();
            Write::delete(pk, sk, Cond::Any)
        })
        .collect();
    for chunk in wipe.chunks(MAX_WRITES) {
        db.write(chunk.to_vec()).await.unwrap();
    }
}

async fn api_keys(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>, clock: &TestClock) {
    let users = Users::new(db.clone(), keys.clone());
    let stores = Stores::new(db.clone(), keys.clone());
    let api = ApiKeys::new(db.clone(), keys.clone());
    let now = clock.now();
    let erin = make_user(&users, "https://issuer.example", "erin", now)
        .await
        .value;
    let store = erin.personal_store_id;
    let new_key = |days: i64| {
        let (_, hash, prefix) = knoggin_crypto::token::new_api_key().unwrap();
        NewApiKey {
            id: Uuid::now_v7(),
            hash,
            prefix,
            key_expires_ms: now + days * MILLIS_PER_DAY,
        }
    };
    let both = [Permission::Read, Permission::Write];

    let k1 = new_key(90);
    let created = api
        .create(
            erin.id,
            store,
            k1.clone(),
            s("marker-key-one"),
            both.to_vec(),
            now,
        )
        .await
        .unwrap();
    let hash = keys::secret_hash(&k1.hash);
    let by_hash = db.get(&keys::key(&hash), ONE).await.unwrap().unwrap();
    let in_store = db
        .get(&keys::store_keys(store), &k1.id.to_string())
        .await
        .unwrap()
        .unwrap();
    let expires = Some(k1.key_expires_ms + 30 * MILLIS_PER_DAY);
    assert_eq!(
        (by_hash.expires_ms, in_store.expires_ms),
        (expires, expires)
    );
    assert_eq!(in_store.doc["hash"], json!(hash));
    assert!(by_hash.doc.get("hash").is_none());
    for d in [&by_hash.doc, &in_store.doc] {
        assert!(!d.to_string().contains("marker"));
        assert_eq!(d["permissions"], json!(["memory:read", "memory:write"]));
    }
    let got = api.get(&k1.hash).await.unwrap().unwrap();
    assert_eq!(got.value, created);
    assert_eq!(got.value.name.expose(), "marker-key-one");
    assert!(got.value.is_active(now));
    assert!(!got.value.is_active(k1.key_expires_ms));
    let listed = api.list(store).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (listed[0].value.hash.as_str(), &listed[0].value.key),
        (hash.as_str(), &created)
    );

    // Last use goes to both copies, once a day, keeping the sealed names.
    api.touch_used(&got, &k1.hash, now).await.unwrap();
    let by_hash2 = db.get(&keys::key(&hash), ONE).await.unwrap().unwrap();
    let in_store2 = db
        .get(&keys::store_keys(store), &k1.id.to_string())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((by_hash2.version, in_store2.version), (2, 2));
    assert_eq!(by_hash2.doc["last_used_day"], json!(utc_day(now)));
    assert_eq!(in_store2.doc["last_used_day"], json!(utc_day(now)));
    assert_eq!(by_hash2.doc["name"], by_hash.doc["name"]);
    assert_eq!(in_store2.doc["name"], in_store.doc["name"]);
    let fresh = api.get(&k1.hash).await.unwrap().unwrap();
    api.touch_used(&fresh, &k1.hash, now + 1).await.unwrap();
    assert_eq!(version(&**db, &keys::key(&hash), ONE).await, Some(2));
    // A stale read never undoes a revocation.
    let revoked = api.revoke(store, k1.id, now + 5).await.unwrap();
    assert_eq!(revoked.revoked_ms, Some(now + 5));
    api.touch_used(&fresh, &k1.hash, now + MILLIS_PER_DAY)
        .await
        .unwrap();
    let after = api.get(&k1.hash).await.unwrap().unwrap();
    assert_eq!(after.value.revoked_ms, Some(now + 5));
    assert_eq!(after.value.last_used_day, Some(utc_day(now)));
    assert!(!after.value.is_active(now + 6));
    // Revoked: both copies, records expire 30 days after the revocation.
    let expires = Some(now + 5 + 30 * MILLIS_PER_DAY);
    assert_eq!(after.expires_ms, expires);
    let in_store3 = api.list(store).await.unwrap().remove(0);
    assert_eq!(in_store3.value.key.revoked_ms, Some(now + 5));
    assert_eq!(in_store3.expires_ms, expires);
    let again = api.revoke(store, k1.id, now + 50).await.unwrap();
    assert_eq!(again.revoked_ms, Some(now + 5));
    assert!(matches!(
        api.revoke(store, Uuid::now_v7(), now).await,
        Err(StateError::NotFound)
    ));

    // Rotation: a new key with the same name and permissions, the old one revoked.
    let k2 = new_key(30);
    api.create(
        erin.id,
        store,
        k2.clone(),
        s("marker-key-two"),
        vec![Permission::Read],
        now,
    )
    .await
    .unwrap();
    let k3 = new_key(90);
    let rotated = api
        .rotate(erin.id, store, k2.id, k3.clone(), now + 10)
        .await
        .unwrap();
    assert_eq!(rotated.name.expose(), "marker-key-two");
    assert_eq!(rotated.permissions, vec![Permission::Read]);
    assert!(
        api.get(&k3.hash)
            .await
            .unwrap()
            .unwrap()
            .value
            .is_active(now + 10)
    );
    assert_eq!(
        api.get(&k2.hash).await.unwrap().unwrap().value.revoked_ms,
        Some(now + 10)
    );
    assert_eq!(api.list(store).await.unwrap().len(), 3);
    let r = api.rotate(erin.id, store, k1.id, new_key(90), now).await;
    assert!(matches!(r, Err(StateError::NotFound)), "{r:?}");

    // A deleted store takes no key.
    stores
        .update(erin.id, store, |r| {
            r.deleted_at_ms = Some(now);
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let k4 = new_key(90);
    let r = api
        .create(erin.id, store, k4.clone(), s("k4"), both.to_vec(), now)
        .await;
    assert!(matches!(r, Err(StateError::StoreDeleted)), "{r:?}");
    assert!(api.get(&k4.hash).await.unwrap().is_none());
    let r = api.rotate(erin.id, store, k3.id, new_key(90), now).await;
    assert!(matches!(r, Err(StateError::StoreDeleted)), "{r:?}");
    stores
        .update(erin.id, store, |r| {
            r.deleted_at_ms = None;
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    api.create(erin.id, store, k4, s("k4"), both.to_vec(), now)
        .await
        .unwrap();

    // At most 1,000 key records per store, revoked ones included.
    let app = new_store("keys app").await;
    let app_id = app.id;
    stores.create_app(erin.id, app, now).await.unwrap();
    let seed: Vec<Write> = (0..crate::api_keys::MAX_KEY_RECORDS)
        .map(|i| {
            Write::put(
                item(&keys::store_keys(app_id), &format!("{i:04}"), 0),
                Cond::Absent,
            )
        })
        .collect();
    for chunk in seed.chunks(MAX_WRITES) {
        db.write(chunk.to_vec()).await.unwrap();
    }
    let r = api
        .create(
            erin.id,
            app_id,
            new_key(90),
            s("one too many"),
            both.to_vec(),
            now,
        )
        .await;
    assert!(matches!(r, Err(StateError::LimitReached)), "{r:?}");
    let r = api
        .rotate(erin.id, app_id, Uuid::now_v7(), new_key(90), now)
        .await;
    assert!(matches!(r, Err(StateError::LimitReached)), "{r:?}");
    let wipe: Vec<Write> = (0..crate::api_keys::MAX_KEY_RECORDS)
        .map(|i| Write::delete(keys::store_keys(app_id), format!("{i:04}"), Cond::Any))
        .collect();
    for chunk in wipe.chunks(MAX_WRITES) {
        db.write(chunk.to_vec()).await.unwrap();
    }
}

async fn sessions(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>, clock: &TestClock) {
    let users = Users::new(db.clone(), keys.clone());
    let sessions = Sessions::new(db.clone(), keys.clone());
    let now = clock.now();
    let frank = make_user(&users, "https://issuer.example", "frank", now)
        .await
        .value;
    let week = 7 * MILLIS_PER_DAY;
    let hash = |n: u8| [n; 32];

    let s1 = sessions
        .create(&hash(1), frank.id, now, week, None, None)
        .await
        .unwrap();
    assert_eq!(s1.expires_ms, Some(now + week));
    let h1 = keys::secret_hash(&hash(1));
    assert_eq!(
        db.get(&keys::session(&h1), ONE)
            .await
            .unwrap()
            .unwrap()
            .expires_ms,
        Some(now + week)
    );
    let idx = db
        .get(&keys::user_sessions(frank.id), &h1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idx.doc, json!({ "created_ms": now }));
    // A local session holds whoever signed in, sealed.
    sessions
        .create(
            &hash(2),
            frank.id,
            now + 1,
            week,
            Some(s("marker-email-local")),
            Some(s("marker-name-local")),
        )
        .await
        .unwrap();
    let h2 = keys::secret_hash(&hash(2));
    assert!(
        !raw(&**db, &keys::session(&h2), ONE)
            .await
            .to_string()
            .contains("marker")
    );
    let s2 = sessions.get(&hash(2)).await.unwrap().unwrap();
    assert_eq!(s2.value.email.unwrap().expose(), "marker-email-local");

    // last_seen_ms at most once a minute.
    sessions.touch(&hash(1), &s1, now + 60_000).await.unwrap();
    assert_eq!(version(&**db, &keys::session(&h1), ONE).await, Some(1));
    sessions.touch(&hash(1), &s1, now + 60_001).await.unwrap();
    let s1b = sessions.get(&hash(1)).await.unwrap().unwrap();
    assert_eq!((s1b.version, s1b.value.last_seen_ms), (2, now + 60_001));
    assert_eq!(
        s1b.expires_ms,
        Some(now + week),
        "the touch keeps the expiry"
    );
    sessions.touch(&hash(1), &s1, now + 120_002).await.unwrap();
    assert_eq!(
        sessions
            .get(&hash(1))
            .await
            .unwrap()
            .unwrap()
            .value
            .last_seen_ms,
        now + 60_001
    );

    // The 21st session removes the oldest.
    for n in 3..=21u8 {
        sessions
            .create(&hash(n), frank.id, now + i64::from(n), week, None, None)
            .await
            .unwrap();
    }
    assert_eq!(
        db.query(&keys::user_sessions(frank.id), "", 100, None)
            .await
            .unwrap()
            .len(),
        20
    );
    assert!(sessions.get(&hash(1)).await.unwrap().is_none());
    assert!(
        db.get(&keys::user_sessions(frank.id), &h1)
            .await
            .unwrap()
            .is_none()
    );
    assert!(sessions.get(&hash(2)).await.unwrap().is_some());

    // Logout, then the end of every session but the current one.
    sessions.delete(&hash(2)).await.unwrap();
    assert!(sessions.get(&hash(2)).await.unwrap().is_none());
    assert!(
        db.get(&keys::user_sessions(frank.id), &h2)
            .await
            .unwrap()
            .is_none()
    );
    sessions.delete(&hash(2)).await.unwrap();
    assert_eq!(
        sessions
            .delete_all(frank.id, Some(&hash(21)))
            .await
            .unwrap(),
        18
    );
    let left = db
        .query(&keys::user_sessions(frank.id), "", 100, None)
        .await
        .unwrap();
    assert_eq!(sks(&left), [keys::secret_hash(&hash(21)).as_str()]);
    // A session past its lifetime is gone.
    sessions
        .create(&hash(99), frank.id, clock.now(), 1000, None, None)
        .await
        .unwrap();
    clock.advance(1000);
    assert!(sessions.get(&hash(99)).await.unwrap().is_none());
}

async fn leases_and_containers(db: &Arc<dyn StateDb>, clock: &TestClock) {
    let leases = Leases::new(db.clone());
    let containers = Containers::new(db.clone());
    let now = clock.now();
    let user = uid();
    assert!(leases.get(user).await.unwrap().is_none());
    let mut lease = Lease::default();
    assert_eq!((lease.state, lease.generation), (LeaseState::Parked, 0));
    assert_eq!(
        lease.next_generation(now),
        u64::try_from(now / 1000).unwrap()
    );
    lease.generation = 5_000_000_000;
    assert_eq!(lease.next_generation(now), 5_000_000_001);

    // A container in the pool, then a claim in one write.
    let cid = Uuid::now_v7();
    let mut c = Container::starting(cid, "1.0.0".into(), String::new(), now, now + 1000);
    containers.put(&c, Cond::Absent).await.unwrap();
    c.state = ContainerState::Ready;
    c.address = Some("10.0.0.2:8443".into());
    containers.put(&c, Cond::Version(1)).await.unwrap();
    let ready = containers.get(cid).await.unwrap().unwrap();
    assert_eq!(ready.version, 2);
    assert!(
        containers
            .list()
            .await
            .unwrap()
            .iter()
            .any(|x| x.value.id == cid)
    );

    let mut claimed = ready.value.clone();
    claimed.state = ContainerState::Claimed;
    claimed.user_id = Some(user);
    let mut lease = Lease::default();
    lease.generation = lease.next_generation(now);
    lease.set_state(LeaseState::Claiming, now);
    lease.container_id = Some(cid);
    lease.address = ready.value.address.clone();
    let mut writes = Leases::writes(user, &lease, Cond::Absent).unwrap();
    writes.push(Containers::write(&claimed, ready.cond()).unwrap());
    db.write(writes).await.unwrap();
    assert!(
        db.get(keys::ACTIVE_LEASES, &user.to_string())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(leases.list_active(1000, None).await.unwrap(), vec![user]);
    // A second router with the same reads loses, and changes nothing.
    let other = uid();
    let mut writes = Leases::writes(other, &lease, Cond::Absent).unwrap();
    writes.push(Containers::write(&claimed, ready.cond()).unwrap());
    conflict(db.write(writes).await);
    assert!(leases.get(other).await.unwrap().is_none());
    assert!(
        db.get(keys::ACTIVE_LEASES, &other.to_string())
            .await
            .unwrap()
            .is_none()
    );

    // Park requests: the first one stays; only parking clears it.
    let mut l = leases.get(user).await.unwrap().unwrap();
    l.value.set_state(LeaseState::Warm, now + 1);
    l.value.add_claim(now + 1);
    assert!(l.value.request_park(ParkReason::Upgrade, now + 2));
    assert!(!l.value.request_park(ParkReason::Drain, now + 3));
    leases.put(user, &l.value, l.cond()).await.unwrap();
    let l = leases.get(user).await.unwrap().unwrap();
    assert_eq!(l.value.park_requested(), Some(ParkReason::Upgrade));
    assert_eq!(l.value.park_requested_ms(), Some(now + 2));
    assert_eq!(l.value.claims(now + 1), 1);
    let mut parked = l.value.clone();
    parked.set_state(LeaseState::Parked, now + 4);
    assert_eq!(parked.park_requested(), None);
    assert_eq!(parked.park_requested_ms(), None);
    leases.put(user, &parked, l.cond()).await.unwrap();
    assert!(
        db.get(keys::ACTIVE_LEASES, &user.to_string())
            .await
            .unwrap()
            .is_none()
    );
    conflict(leases.put(user, &parked, l.cond()).await);

    // claims and abnormal ends keep the last 10 minutes.
    let ten = crate::record::LEASE_WINDOW_MS;
    let mut l = Lease::default();
    l.add_claim(now);
    l.add_claim(now + ten / 2);
    l.add_claim(now + ten);
    assert_eq!(l.claims(now + ten), 2);
    l.add_abnormal_end(now, EndKind::Memory);
    l.add_abnormal_end(now + 1, EndKind::Failed);
    assert_eq!(l.abnormal_ends(now + ten).len(), 1);
    l.add_abnormal_end(now + ten + 1, EndKind::Failed);
    assert_eq!(l.abnormal_ends(now + ten + 1).len(), 1);
    leases.put(user, &l, Cond::Any).await.unwrap();
    assert_eq!(leases.get(user).await.unwrap().unwrap().value, l);

    // The first stop reason stays.
    let mut c = containers.get(cid).await.unwrap().unwrap();
    assert!(c.value.set_stop_reason(StopReason::Unresponsive));
    assert!(!c.value.set_stop_reason(StopReason::Rollout));
    containers.put(&c.value, c.cond()).await.unwrap();
    let c2 = containers.get(cid).await.unwrap().unwrap();
    assert_eq!(c2.value.stop_reason(), Some(StopReason::Unresponsive));
    conflict(containers.delete(cid, c.cond()).await);
    containers.delete(cid, c2.cond()).await.unwrap();
    assert!(containers.get(cid).await.unwrap().is_none());
}

async fn leader_and_rollout(db: &Arc<dyn StateDb>, clock: &TestClock) {
    let meta = Meta::new(db.clone());
    let now = clock.now();
    assert!(meta.leader().await.unwrap().is_none());
    let lead = Leader {
        router_id: "r1".into(),
        build: "1.0.0".into(),
        until_ms: now + 30_000,
    };
    meta.put_leader(&lead, Cond::Absent).await.unwrap();
    let held = meta.leader().await.unwrap().unwrap();
    assert_eq!(held.value, lead);
    let rival = Leader {
        router_id: "r2".into(),
        ..lead.clone()
    };
    conflict(meta.put_leader(&rival, Cond::Absent).await);
    clock.advance(30_000);
    assert!(
        meta.leader().await.unwrap().is_none(),
        "an expired leader is absent"
    );
    let rival = Leader {
        until_ms: clock.now() + 30_000,
        ..rival
    };
    meta.put_leader(&rival, Cond::Absent).await.unwrap();
    assert_eq!(meta.leader().await.unwrap().unwrap().value.router_id, "r2");

    assert!(meta.rollout().await.unwrap().is_none());
    let spec = crate::record::BuildSpec {
        build: "1.0.0".into(),
        spec: String::new(),
    };
    let r = Rollout {
        claim: spec.clone(),
        desired: spec,
        retire_before_ms: None,
    };
    meta.put_rollout(&r, Cond::Absent).await.unwrap();
    let got = meta.rollout().await.unwrap().unwrap();
    assert_eq!(got.value, r);
    conflict(meta.put_rollout(&r, Cond::Absent).await);
    meta.put_rollout(&r, got.cond()).await.unwrap();
}

async fn erase_cases(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>, clock: &TestClock) {
    let users = Users::new(db.clone(), keys.clone());
    let stores = Stores::new(db.clone(), keys.clone());
    let api = ApiKeys::new(db.clone(), keys.clone());
    let sessions = Sessions::new(db.clone(), keys.clone());
    let leases = Leases::new(db.clone());
    let days = 7u32;
    let grace = i64::from(days) * MILLIS_PER_DAY + 10 * 60 * 1000;
    let t0 = clock.now();

    let gina = make_user(&users, "https://issuer.example", "gina", t0)
        .await
        .value;
    let personal = gina.personal_store_id;
    let app = new_store("marker-app-gina").await;
    let app_id = app.id;
    stores.create_app(gina.id, app, t0).await.unwrap();
    let (_, hash, prefix) = knoggin_crypto::token::new_api_key().unwrap();
    let key = NewApiKey {
        id: Uuid::now_v7(),
        hash,
        prefix,
        key_expires_ms: t0 + 1000 * MILLIS_PER_DAY,
    };
    api.create(
        gina.id,
        app_id,
        key.clone(),
        s("k"),
        vec![Permission::Read],
        t0,
    )
    .await
    .unwrap();
    sessions
        .create(&[7; 32], gina.id, t0, 1000 * MILLIS_PER_DAY, None, None)
        .await
        .unwrap();
    let mut lease = Lease::default();
    lease.set_state(LeaseState::Warm, t0);
    leases.put(gina.id, &lease, Cond::Absent).await.unwrap();

    stores
        .update(gina.id, app_id, |r| {
            r.deleted_at_ms = Some(t0);
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();

    // Exactly at the end of the grace period: nothing is due yet.
    let mut called = Vec::new();
    let report = erase(db.clone(), t0 + grace, days, |s| {
        called.push(s);
        async { Ok::<(), ()>(()) }
    })
    .await
    .unwrap();
    assert_eq!(report, crate::EraseReport::default());
    assert!(called.is_empty());

    // A failing snapshot delete leaves every record for the next run.
    let report = erase(db.clone(), t0 + grace + 1, days, |_| async {
        Err::<(), _>("bucket down")
    })
    .await
    .unwrap();
    assert_eq!((report.stores_failed, report.stores_erased), (1, 0));
    assert!(stores.get(app_id).await.unwrap().is_some());

    let report = erase(db.clone(), t0 + grace + 1, days, |s| {
        called.push(s);
        async { Ok::<(), ()>(()) }
    })
    .await
    .unwrap();
    assert_eq!(called, vec![app_id]);
    assert_eq!((report.stores_erased, report.users_erased), (1, 0));
    assert!(
        db.get(&keys::user_stores(gina.id), &app_id.to_string())
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.get(&keys::store(app_id), ONE).await.unwrap().is_none());
    assert!(
        db.get(&keys::key(&keys::secret_hash(&key.hash)), ONE)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db.query(&keys::store_keys(app_id), "", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.query(&keys::user_store_names(gina.id), "", 10, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(stores.get(personal).await.unwrap().is_some());
    assert!(users.get(gina.id).await.unwrap().is_some());

    // The user's mark erases its stores that have none, then the user.
    let t1 = t0 + grace + 2;
    users
        .update(gina.id, |u| {
            u.deleted_at_ms = Some(t1);
            u.deleted_by = Some(crate::record::DeletedBy::Admin);
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let sub_hash = users.get(gina.id).await.unwrap().unwrap().value.sub_hash;
    let report = erase(db.clone(), t1 + grace, days, |_| async { Ok::<(), ()>(()) })
        .await
        .unwrap();
    assert_eq!(report, crate::EraseReport::default(), "not due yet");
    // A user with a store left (its snapshots would not delete) stays.
    let report = erase(db.clone(), t1 + grace + 1, days, |_| async {
        Err::<(), _>(())
    })
    .await
    .unwrap();
    assert_eq!((report.stores_failed, report.users_erased), (1, 0));
    assert!(users.get(gina.id).await.unwrap().is_some());
    let report = erase(db.clone(), t1 + grace + 1, days, |_| async {
        Ok::<(), ()>(())
    })
    .await
    .unwrap();
    assert_eq!((report.stores_erased, report.users_erased), (1, 1));
    for (pk, sk) in [
        (keys::user(gina.id), ONE.to_owned()),
        (keys::USERS.to_owned(), gina.id.to_string()),
        (keys::sub(&sub_hash), ONE.to_owned()),
        (keys::lease(gina.id), ONE.to_owned()),
        (keys::ACTIVE_LEASES.to_owned(), gina.id.to_string()),
        (keys::session(&keys::secret_hash(&[7; 32])), ONE.to_owned()),
        (keys::store(personal), ONE.to_owned()),
    ] {
        assert!(
            db.get(&pk, &sk).await.unwrap().is_none(),
            "{pk} {sk} is erased"
        );
    }
    for pk in [
        keys::user_sessions(gina.id),
        keys::user_stores(gina.id),
        keys::user_store_names(gina.id),
    ] {
        assert!(
            db.query(&pk, "", 10, None).await.unwrap().is_empty(),
            "{pk} is empty"
        );
    }
    // The same person signs in again and gets a new user.
    let back = make_user(&users, "https://issuer.example", "gina", t1).await;
    assert_ne!(back.value.id, gina.id);

    // The local user is never erased, only its due stores; it then gets a new personal store.
    let local = make_user(
        &users,
        crate::users::LOCAL_IDENTITY,
        crate::users::LOCAL_IDENTITY,
        t1,
    )
    .await;
    users
        .update(local.value.id, |u| {
            u.deleted_at_ms = Some(t1);
            u.deleted_by = Some(crate::record::DeletedBy::Owner);
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let report = erase(db.clone(), t1 + grace + 1, days, |_| async {
        Ok::<(), ()>(())
    })
    .await
    .unwrap();
    assert_eq!((report.stores_erased, report.users_erased), (1, 0));
    let local = users.get(local.value.id).await.unwrap().unwrap();
    assert!(stores.list(local.value.id).await.unwrap().is_empty());
    let fresh = new_store("Personal").await;
    let fresh_id = fresh.id;
    let local = users
        .replace_personal_store(&local, fresh, t1 + grace + 2)
        .await
        .unwrap();
    assert_eq!(local.value.personal_store_id, fresh_id);
    assert_eq!(local.value.deleted_at_ms, None);
    let stored = users.get(local.value.id).await.unwrap().unwrap();
    assert_eq!(stored.value, local.value);
    assert!(stores.get(fresh_id).await.unwrap().is_some());
}

async fn rewrap(db: &Arc<dyn StateDb>, keys: &Arc<FieldKeys>) {
    let old = test_provider();
    let mut set = test_key_set();
    set.rotate().unwrap();
    let new_kek = set.current_kek_id();
    let rotated = LocalKeyProvider::new(set);
    let users = Users::new(db.clone(), keys.clone());
    let stores = Stores::new(db.clone(), keys.clone());
    let all_stores = || async {
        let mut out = Vec::new();
        for u in users.list(1000, None).await.unwrap() {
            out.extend(stores.list(u.value.id).await.unwrap());
        }
        out
    };
    let before = all_stores().await;
    assert!(!before.is_empty());
    let n = rewrap_master_key(db.clone(), keys.clone(), &rotated)
        .await
        .unwrap();
    assert_eq!(n, before.len());
    let after = all_stores().await;
    for (b, a) in before.iter().zip(&after) {
        assert_eq!(a.value.kek_id, new_kek);
        let old_key = old
            .unwrap(b.value.id, &b.value.wrapped().unwrap())
            .await
            .unwrap();
        let new_key = rotated
            .unwrap(a.value.id, &a.value.wrapped().unwrap())
            .await
            .unwrap();
        assert_eq!(old_key.expose(), new_key.expose());
        assert_eq!(a.value.name, b.value.name);
    }
    let rec = raw(&**db, "meta", "field-key").await;
    assert_eq!(rec["kek_id"], json!(new_kek));
    let reopened = Meta::new(db.clone()).field_keys(&rotated).await.unwrap();
    let sealed = keys.seal_field("p", "s", "f", "same key").unwrap();
    assert_eq!(
        reopened.open_field("p", "s", "f", &sealed).unwrap(),
        "same key"
    );
}
