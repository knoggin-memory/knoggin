// SPDX-License-Identifier: AGPL-3.0-only
//! The local state file: sequence numbers, the 3-file rule, expired items left out,
//! corruption, a wrong key, crash safety of the rewrite, use-previous and rotation.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use knoggin_core::StoreId;
use knoggin_crypto::testing::{open_snapshot, test_key_set, test_provider};
use knoggin_crypto::{KeyError, LocalKeyProvider, MasterKeySet};
use knoggin_state::local::{previous_state_file, set_aside_newest};
use knoggin_state::testing::{TestClock, test_field_keys};
use knoggin_state::{
    Cond, Item, LocalFileDb, NewStore, NewUser, StateDb, StateError, Users, Write,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct StateItemV1 {
    pk: String,
    sk: String,
    version: u64,
    expires_ms: Option<i64>,
    doc: String,
}

fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn name(seq: u64) -> String {
    format!("state-{seq:010}.kgs")
}

fn newest(dir: &Path) -> PathBuf {
    let n = files(dir)
        .into_iter()
        .rfind(|f| f.ends_with(".kgs"))
        .unwrap();
    dir.join(n)
}

/// The items in a state file, read with the test key set.
fn content(path: &Path) -> Vec<StateItemV1> {
    let (header, bytes) = open_snapshot(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        (header.store_id, header.generation),
        (StoreId::default(), 0)
    );
    assert_eq!(header.content_format, 1);
    let (items, rest) = postcard::take_from_bytes::<Vec<StateItemV1>>(&bytes).unwrap();
    assert!(rest.is_empty());
    items
}

async fn put(db: &LocalFileDb, sk: &str, n: i64) -> Result<(), StateError> {
    db.write(vec![Write::put(
        Item::new("p", sk, json!({ "n": n })),
        Cond::Any,
    )])
    .await
}

async fn open(dir: &Path, clock: &TestClock) -> Result<Option<LocalFileDb>, StateError> {
    LocalFileDb::open(dir, &test_provider(), clock.clock()).await
}

async fn create(dir: &Path, clock: &TestClock) -> LocalFileDb {
    LocalFileDb::create(dir, &test_provider(), clock.clock())
        .await
        .unwrap()
}

#[tokio::test]
async fn sequence_numbers_and_the_three_newest_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("state");
    let clock = TestClock::new();
    let db = create(&dir, &clock).await;
    assert_eq!(files(&dir), [name(1)]);
    assert!(content(&dir.join(name(1))).is_empty());
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join(name(1)))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    for n in 0..5 {
        put(&db, &format!("k{n}"), n).await.unwrap();
    }
    assert_eq!(files(&dir), [name(4), name(5), name(6)]);
    // A failed condition writes no file.
    let r = db
        .write(vec![Write::put(
            Item::new("p", "k0", json!({})),
            Cond::Absent,
        )])
        .await;
    assert!(matches!(r, Err(StateError::Conflict)));
    assert_eq!(files(&dir), [name(4), name(5), name(6)]);
    // The file holds the items, sorted, with their versions.
    let items = content(&dir.join(name(6)));
    let keys: Vec<_> = items
        .iter()
        .map(|i| (i.pk.as_str(), i.sk.as_str()))
        .collect();
    assert_eq!(
        keys,
        [
            ("p", "k0"),
            ("p", "k1"),
            ("p", "k2"),
            ("p", "k3"),
            ("p", "k4")
        ]
    );
    assert!(items.iter().all(|i| i.version == 1));
    assert_eq!(items[4].doc, r#"{"n":4}"#);
    drop(db);

    // Reopened, the newest file is the state and the next write is sequence 7.
    let db = open(&dir, &clock).await.unwrap().unwrap();
    assert_eq!(
        db.get("p", "k3").await.unwrap().unwrap().doc,
        json!({ "n": 3 })
    );
    put(&db, "k0", 9).await.unwrap();
    assert_eq!(files(&dir), [name(5), name(6), name(7)]);
    assert_eq!(db.get("p", "k0").await.unwrap().unwrap().version, 2);
    // create refuses a directory that has a state file.
    let r = LocalFileDb::create(&dir, &test_provider(), clock.clock()).await;
    assert!(matches!(r, Err(StateError::Invalid(_))));
}

#[tokio::test]
async fn no_state_file_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    assert!(open(tmp.path(), &clock).await.unwrap().is_none());
}

#[tokio::test]
async fn expired_items_are_left_out_of_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    let now = clock.now();
    db.write(vec![
        Write::put(
            Item::new("p", "short", json!({})).expiring(Some(now + 10)),
            Cond::Any,
        ),
        Write::put(
            Item::new("p", "long", json!({})).expiring(Some(now + 1000)),
            Cond::Any,
        ),
    ])
    .await
    .unwrap();
    let items = content(&newest(tmp.path()));
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].expires_ms, Some(now + 1000));
    clock.advance(10);
    put(&db, "other", 0).await.unwrap();
    let sks: Vec<_> = content(&newest(tmp.path()))
        .into_iter()
        .map(|i| i.sk)
        .collect();
    assert_eq!(sks, ["long", "other"]);
    // An item that expires while the file is closed is left out when it is read.
    drop(db);
    clock.advance(1000);
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert!(db.get("p", "long").await.unwrap().is_none());
    assert!(db.get("p", "other").await.unwrap().is_some());
}

#[tokio::test]
async fn a_corrupt_newest_file_is_refused_without_falling_back() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    put(&db, "a", 1).await.unwrap();
    put(&db, "b", 2).await.unwrap();
    drop(db);
    let path = newest(tmp.path());
    let good = fs::read(&path).unwrap();
    // A flipped bit in the header, in the body, a cut-off file, bytes appended, garbage.
    let mut cases = Vec::new();
    for at in [5, 40, 60, 70, good.len() - 1] {
        let mut b = good.clone();
        b[at] ^= 1;
        cases.push(b);
    }
    cases.push(good[..good.len() - 1].to_vec());
    cases.push([good.clone(), vec![0]].concat());
    cases.push(b"not a state file".to_vec());
    for (i, bytes) in cases.into_iter().enumerate() {
        fs::write(&path, &bytes).unwrap();
        let r = open(tmp.path(), &clock).await;
        assert!(matches!(r, Err(StateError::Corrupt(_))), "case {i}: {r:?}");
        assert_eq!(files(tmp.path()).len(), 3, "nothing is removed or renamed");
    }
    // The file of another sequence under this name is refused too.
    fs::copy(tmp.path().join(name(2)), &path).unwrap();
    assert!(matches!(
        open(tmp.path(), &clock).await,
        Err(StateError::Corrupt(_))
    ));
    fs::write(&path, &good).unwrap();
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert!(db.get("p", "b").await.unwrap().is_some());
}

#[tokio::test]
async fn a_wrong_master_key_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    put(&db, "a", 1).await.unwrap();
    drop(db);
    let other = LocalKeyProvider::new(MasterKeySet::generate().unwrap());
    let r = LocalFileDb::open(tmp.path(), &other, clock.clock()).await;
    assert!(
        matches!(r, Err(StateError::Key(KeyError::UnknownMasterKey { .. }))),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_crash_during_a_rewrite_leaves_the_last_state() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    put(&db, "a", 1).await.unwrap();
    drop(db);
    // The process died after writing part of the next file.
    let tmp_file = tmp.path().join(format!("{}.tmp", name(3)));
    fs::write(&tmp_file, b"half a file").unwrap();
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert!(
        !tmp_file.exists(),
        "a leftover temporary file is removed at open"
    );
    assert_eq!(
        db.get("p", "a").await.unwrap().unwrap().doc,
        json!({ "n": 1 })
    );
    put(&db, "b", 2).await.unwrap();
    assert_eq!(files(tmp.path()), [name(1), name(2), name(3)]);

    // A file under the next name already exists: the write takes the one after it.
    fs::copy(tmp.path().join(name(3)), tmp.path().join(name(4))).unwrap();
    put(&db, "c", 3).await.unwrap();
    assert_eq!(files(tmp.path()), [name(3), name(4), name(5)]);
    drop(db);
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert!(db.get("p", "c").await.unwrap().is_some());
}

#[tokio::test]
async fn a_write_that_cannot_be_saved_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    put(&db, "a", 1).await.unwrap();
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let r = put(&db, "a", 2).await;
    let r2 = db.write(vec![Write::delete("p", "a", Cond::Any)]).await;
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(r, Err(StateError::Unavailable(_))), "{r:?}");
    assert!(matches!(r2, Err(StateError::Unavailable(_))), "{r2:?}");
    let a = db.get("p", "a").await.unwrap().unwrap();
    assert_eq!((a.version, a.doc), (1, json!({ "n": 1 })));
    assert_eq!(files(tmp.path()), [name(1), name(2)]);
    put(&db, "a", 3).await.unwrap();
    assert_eq!(db.get("p", "a").await.unwrap().unwrap().version, 2);
    drop(db);
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert_eq!(
        db.get("p", "a").await.unwrap().unwrap().doc,
        json!({ "n": 3 })
    );
}

#[tokio::test]
async fn use_previous_sets_the_newest_file_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = Arc::new(create(tmp.path(), &clock).await);
    let keys = Arc::new(test_field_keys());
    let users = Users::new(db.clone(), keys);
    let store = StoreId::from_uuid(uuid::Uuid::now_v7());
    let (_, wrapped) = knoggin_crypto::KeyProvider::generate(&test_provider(), store)
        .await
        .unwrap();
    let new = NewUser {
        id: knoggin_core::UserId::from_uuid(uuid::Uuid::now_v7()),
        issuer: "local".into(),
        subject: "local".to_owned().into(),
        email: None,
        email_verified: false,
        name: None,
    };
    let personal = NewStore {
        id: store,
        name: "Personal".to_owned().into(),
        wrapped,
    };
    users.create(new, personal, clock.now()).await.unwrap();
    clock.advance(5);
    put(&db, "later", 1).await.unwrap();
    drop(users);
    drop(db);

    fs::write(newest(tmp.path()), b"broken").unwrap();
    assert!(matches!(
        open(tmp.path(), &clock).await,
        Err(StateError::Corrupt(_))
    ));
    let prev = previous_state_file(tmp.path(), &test_provider())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(prev.path, tmp.path().join(name(2)));
    assert_eq!(prev.store_ids, vec![store]);
    assert_eq!(prev.key_count, 0);
    assert_eq!(prev.created_ms, u64::try_from(clock.now() - 5).unwrap());
    let bad = set_aside_newest(tmp.path()).await.unwrap().unwrap();
    assert_eq!(bad, tmp.path().join(format!("{}.bad", name(3))));
    let db = open(tmp.path(), &clock).await.unwrap().unwrap();
    assert!(
        db.get("p", "later").await.unwrap().is_none(),
        "what came after is undone"
    );
    put(&db, "again", 1).await.unwrap();
    assert_eq!(
        files(tmp.path()),
        [name(1), name(2), name(3), format!("{}.bad", name(3))]
    );
}

#[tokio::test]
async fn rotation_wraps_the_file_key_again() {
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db = create(tmp.path(), &clock).await;
    put(&db, "a", 1).await.unwrap();
    let mut set = test_key_set();
    set.rotate().unwrap();
    let kek = set.current_kek_id();
    let rotated = LocalKeyProvider::new(set);
    db.rewrap(&rotated).await.unwrap();
    let bytes = fs::read(newest(tmp.path())).unwrap();
    assert!(
        open_snapshot(&bytes).is_err(),
        "the old key set alone cannot open it"
    );
    let (header, _) = knoggin_crypto::container::read_header(&bytes).unwrap();
    assert_eq!(header.kek_id, kek);
    put(&db, "b", 2).await.unwrap();
    drop(db);
    let db = LocalFileDb::open(tmp.path(), &rotated, clock.clock())
        .await
        .unwrap()
        .unwrap();
    assert!(db.get("p", "a").await.unwrap().is_some());
    assert!(db.get("p", "b").await.unwrap().is_some());
    let r = open(tmp.path(), &clock).await;
    assert!(
        matches!(r, Err(StateError::Key(KeyError::UnknownMasterKey { .. }))),
        "{r:?}"
    );
}

#[tokio::test]
async fn no_personal_field_is_readable_in_the_decrypted_state_file() {
    use knoggin_state::record::Permission;
    use knoggin_state::{ApiKeys, NewApiKey, Sessions, Stores};
    let tmp = tempfile::tempdir().unwrap();
    let clock = TestClock::new();
    let db: Arc<LocalFileDb> = Arc::new(create(tmp.path(), &clock).await);
    let keys = Arc::new(test_field_keys());
    let now = clock.now();
    let store = StoreId::from_uuid(uuid::Uuid::now_v7());
    let (_, wrapped) = knoggin_crypto::KeyProvider::generate(&test_provider(), store)
        .await
        .unwrap();
    let new = NewUser {
        id: knoggin_core::UserId::from_uuid(uuid::Uuid::now_v7()),
        issuer: "https://issuer.example".into(),
        subject: "marker-subject".to_owned().into(),
        email: Some("marker-email@example.com".to_owned().into()),
        email_verified: true,
        name: Some("marker-user-name".to_owned().into()),
    };
    let personal = NewStore {
        id: store,
        name: "marker-store-name".to_owned().into(),
        wrapped,
    };
    let (user, _) = Users::new(db.clone(), keys.clone())
        .create(new, personal, now)
        .await
        .unwrap();
    let uid = user.value.id;
    Stores::new(db.clone(), keys.clone())
        .update(uid, store, |s| {
            s.last_write_client = Some("marker-client".to_owned().into());
            Ok::<_, StateError>(())
        })
        .await
        .unwrap();
    let (secret, hash, prefix) = knoggin_crypto::token::new_api_key().unwrap();
    let key = NewApiKey {
        id: uuid::Uuid::now_v7(),
        hash,
        prefix,
        key_expires_ms: now + 1_000_000,
    };
    ApiKeys::new(db.clone(), keys.clone())
        .create(
            uid,
            store,
            key,
            "marker-key-name".to_owned().into(),
            vec![Permission::Read],
            now,
        )
        .await
        .unwrap();
    Sessions::new(db.clone(), keys)
        .create(
            &[1; 32],
            uid,
            now,
            1_000_000,
            Some("marker-session-email".to_owned().into()),
            Some("marker-session-name".to_owned().into()),
        )
        .await
        .unwrap();
    let path = newest(tmp.path());
    let file = fs::read(&path).unwrap();
    let plain = open_snapshot(&file).unwrap().1;
    for bytes in [&file, &plain] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("marker"), "a marker in the clear");
        assert!(!text.contains(&secret), "the key itself");
    }
    // The records are all there, the hashes and IDs readable.
    let items = content(&path);
    assert!(items.iter().any(|i| i.pk == format!("user#{uid}")));
    assert!(items.iter().any(|i| i.pk.starts_with("key#")));
    assert!(items.iter().any(|i| i.pk.starts_with("session#")));
}
