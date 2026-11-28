// SPDX-License-Identifier: AGPL-3.0-only
//! The frozen snapshots of content format 1 (`tests/fixtures/snapshots/v1/`). Every build
//! opens them and checks the decoded store against values written out here, so that an
//! accidental change of the content or container format fails. The ignored test at the
//! end wrote them once; it refuses to overwrite a file.
//!
//! This test lives in `knoggin-crypto` because `knoggin-core` cannot depend on it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::model::{AccessKind, EdgeType, EmbeddingState, NamespacePolicy, Phase};
use knoggin_core::ops::{
    AccessHit, DeleteNamespace, ForgetMemory, Idempotent, ReinforceMemory, StoreMemory,
    SupersedeMemory, UpdatePolicy, WriteContext,
};
use knoggin_core::snapshot::{MAX_CONTENT_FORMAT, MIN_CONTENT_FORMAT, SnapshotName};
use knoggin_core::testing::FakeProvider;
use knoggin_core::transfer::{OnConflict, TransferRecord};
use knoggin_core::{MemoryId, Store, StoreId, Timestamp};
use knoggin_crypto::testing::{open_snapshot, seal_snapshot, test_key_set};
use serde_json::json;

/// The `written_by` of the fixtures: the knoggin-core version that wrote them.
const WRITTEN_BY: &str = "0.1.0";
const T0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
/// When the fixtures were sealed (the header's `created_ms`).
const SEALED_MS: u64 = 1_700_086_400_000;

/// The three fixtures: file, store ID, snapshot name.
const EMPTY: (&str, [u8; 16], (u64, u64)) = ("empty.kgs", store_bytes(1), (1, 1));
const SMALL: (&str, [u8; 16], (u64, u64)) = ("small.kgs", store_bytes(2), (1, 2));
const FULL: (&str, [u8; 16], (u64, u64)) = ("full.kgs", store_bytes(3), (2, 7));

const fn store_bytes(n: u8) -> [u8; 16] {
    [
        0x01, 0x8b, 0xcf, 0xe5, 0x68, 0x00, 0x70, 0x00, 0x80, 0, 0, 0, 0, 0, 0, n,
    ]
}

fn mid(n: u8) -> MemoryId {
    let mut b = store_bytes(0);
    b[8] = 0x90;
    b[15] = n;
    MemoryId::from_bytes(b)
}

fn t(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/snapshots/v1")
}

/// Opens one fixture as a host does: check the name, the format, decrypt, decode.
/// Returns the content and the store.
fn open((file, store, (generation, sequence)): (&str, [u8; 16], (u64, u64))) -> (Vec<u8>, Store) {
    let bytes = std::fs::read(dir().join(file)).unwrap();
    let (header, content) = open_snapshot(&bytes).unwrap();
    let store = StoreId::from_bytes(store);
    header
        .expect(store, SnapshotName::new(generation, sequence).unwrap())
        .unwrap();
    header
        .check_content_format(MIN_CONTENT_FORMAT, MAX_CONTENT_FORMAT)
        .unwrap();
    assert_eq!(header.kek_id, test_key_set().current_kek_id());
    assert_eq!(header.created_ms, SEALED_MS);
    let decoded = Store::decode(header.content_format, &content).unwrap();
    assert_eq!(decoded.id(), store);
    assert_eq!(
        decoded.encode_as(WRITTEN_BY),
        content,
        "{file}: re-encoded bytes differ"
    );
    assert!(decoded.invariant_violations_for_test().is_empty());
    (content, decoded)
}

fn edge_types(s: &Store) -> BTreeSet<EdgeType> {
    s.graph().iter().map(|e| e.edge_type).collect()
}

#[test]
fn empty_v1() {
    let (content, s) = open(EMPTY);
    assert_eq!(content.len(), 169);
    assert_eq!(s.created_at(), t(T0));
    assert_eq!(
        (s.model().id.as_str(), s.model().dimensions),
        ("fake-64", 64)
    );
    assert_eq!(
        s.namespaces().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        ["default"]
    );
    assert_eq!(s.memories().count(), 0);
    assert_eq!(s.graph().len(), 0);
    assert_eq!(
        (
            s.changes().content,
            s.changes().bookkeeping,
            s.changes().forgets
        ),
        (0, 0, 0)
    );
}

#[test]
fn small_v1() {
    let (_, s) = open(SMALL);
    assert_eq!(s.memories().count(), 2);
    assert_eq!(s.vectors().len(), 1);
    assert_eq!(edge_types(&s), BTreeSet::from([EdgeType::Temporal]));
    let m1 = s.memory(mid(1)).unwrap();
    assert_eq!(m1.summary, "Maya prefers window seats");
    assert_eq!(m1.embedding, EmbeddingState::Ready);
    assert_eq!(
        s.vector(mid(1)).unwrap(),
        FakeProvider::new().vector(&m1.summary)
    );
    assert_eq!(s.memory(mid(2)).unwrap().embedding, EmbeddingState::Pending);
    assert_eq!(
        (
            s.changes().content,
            s.changes().bookkeeping,
            s.changes().forgets
        ),
        (2, 0, 0)
    );
}

#[test]
fn full_v1() {
    let (_, mut s) = open(FULL);
    let names: Vec<_> = s
        .namespaces()
        .map(|n| (n.id.get(), n.name.clone()))
        .collect();
    assert_eq!(names, [(1, "default".to_owned()), (2, "work".to_owned())]);
    let work = s.namespace_by_name("work").unwrap();
    assert_eq!(work.policy, work_policy());
    assert_eq!(s.memories().count(), 8);
    assert_eq!(s.graph().len(), 16);
    assert_eq!(s.vectors().len(), 4);
    assert_eq!(edge_types(&s), EdgeType::ALL.into_iter().collect());
    assert!(s.graph().iter().any(|e| !e.auto_created) && s.graph().iter().any(|e| e.auto_created));
    assert_eq!(
        (
            s.changes().content,
            s.changes().bookkeeping,
            s.changes().forgets
        ),
        (15, 4, 1)
    );
    assert!(s.memory(mid(10)).is_none(), "the forgotten memory is gone");
    let edge = |source: u8, edge_type, target: u8| {
        s.graph()
            .iter()
            .find(|e| (e.source, e.edge_type, e.target) == (mid(source), edge_type, mid(target)))
            .copied()
    };
    let parent = edge(1, EdgeType::ParentChild, 2).unwrap();
    assert_eq!(
        (parent.weight, parent.auto_created, parent.created_at),
        (1.0, false, t(T0 + 2 * MIN))
    );
    assert!(edge(3, EdgeType::Supersedes, 1).is_some_and(|e| !e.auto_created));
    assert_eq!(edge(2, EdgeType::Entity, 1).unwrap().weight, 2.0 / 3.0);
    assert!(edge(8, EdgeType::Associative, 1).is_some_and(|e| e.auto_created));

    let m1 = s.memory(mid(1)).unwrap();
    assert_eq!(
        m1.full_text.as_deref(),
        Some("Maya prefers window seats on long trains")
    );
    assert_eq!(m1.labels.entities, ["Maya Smith", "Lisbon"]);
    assert_eq!(m1.labels.topics, ["Seats"]);
    assert_eq!(m1.labels.emotions, ["happy"]);
    assert_eq!(
        m1.tags.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
        ["travel", "trips/2024"]
    );
    assert_eq!(m1.retrieved_at, Some(t(T0 + 20 * MIN)));
    assert_eq!(m1.access_count, 1);
    assert_eq!(m1.client.as_deref(), Some("fixture-client"));
    let m2 = s.memory(mid(2)).unwrap();
    assert_eq!(
        m2.access_history.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [AccessKind::Associative, AccessKind::Reinforcement]
    );
    assert_eq!(m2.reinforced_at, Some(t(T0 + 21 * MIN)));
    assert!(m2.stability > 3.7145);
    let m4 = s.memory(mid(4)).unwrap();
    assert_eq!(
        (m4.embedding, m4.embed_failures),
        (EmbeddingState::Failed, 3)
    );
    let m5 = s.memory(mid(5)).unwrap();
    assert_eq!(
        (m5.phase, m5.full_text.is_none(), m5.is_permastore),
        (Phase::Summary, true, true)
    );
    assert_eq!(m5.saved_by, "import");
    let m6 = s.memory(mid(6)).unwrap();
    assert_eq!(
        (m6.phase, m6.embedding, m6.summary.as_str()),
        (Phase::Ghost, EmbeddingState::None, "")
    );
    let m7 = s.memory(mid(7)).unwrap();
    assert_eq!(
        (m7.saved_by.as_str(), m7.client.as_deref()),
        ("api_key:key1", None)
    );
    assert_eq!(m7.namespace, work.id);

    // The full-text index was rebuilt from the records.
    let default = s.namespace_by_name("default").unwrap().id;
    let hits = s.fts().search(default, "long", 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [mid(1)]);

    // Both idempotency entries replay; on_open would drop them a day later.
    for (key, hash) in [("save-8", [8_u8; 32]), ("forget-9", [9; 32])] {
        let replay = s
            .run_idempotent(
                key,
                hash,
                t(T0 + 60 * MIN),
                |_: &mut Store| -> Result<((), Vec<MemoryId>), _> { panic!("{key} ran again") },
            )
            .unwrap();
        assert!(
            matches!(replay, Idempotent::Replayed(ref b) if !b.is_empty()),
            "{key}"
        );
    }
    // The failed memory is queued again only by on_open, never by decode.
    s.on_open(t(T0 + 60 * MIN));
    assert_eq!(s.memory(mid(4)).unwrap().embedding, EmbeddingState::Pending);
}

fn work_policy() -> NamespacePolicy {
    serde_json::from_value(json!({
        "initialStability": 10.0, "defaultDifficulty": 4.0,
        "phaseThresholds": { "fullToSummary": 0.6, "summaryToGhost": 0.25, "ghostToDelete": 0.04 },
        "permastoreThreshold": 1000.0, "decayRateMultiplier": 0.5,
    }))
    .unwrap()
}

fn ctx() -> WriteContext {
    WriteContext {
        saved_by: "local".into(),
        client: Some("fixture-client".into()),
    }
}

fn save(s: &mut Store, n: u8, body: serde_json::Value, vector: bool, at: i64) {
    let req: StoreMemory = serde_json::from_value(body).unwrap();
    let vector = vector.then(|| FakeProvider::new().vector(&req.summary));
    s.store_memory(req, mid(n), vector, &ctx(), t(at)).unwrap();
}

fn new_store(n: u8) -> Store {
    Store::create(
        StoreId::from_bytes(store_bytes(n)),
        FakeProvider::new().identity().clone(),
        t(T0),
    )
}

fn small() -> Store {
    let mut s = new_store(2);
    save(
        &mut s,
        1,
        json!({ "summary": "Maya prefers window seats" }),
        true,
        T0 + MIN,
    );
    save(
        &mut s,
        2,
        json!({ "summary": "Lunch with Ana on Friday" }),
        false,
        T0 + 2 * MIN,
    );
    s
}

/// Every field of content format 1 in use.
fn full() -> Store {
    let mut s = new_store(3);
    let ns = |name: &str| serde_json::from_value(json!({ "name": name })).unwrap();
    s.create_namespace(ns("work"), &ctx(), t(T0)).unwrap();
    s.create_namespace(ns("scratch"), &ctx(), t(T0)).unwrap();
    let del = DeleteNamespace {
        name: "scratch".into(),
        force: false,
        dry_run: false,
    };
    s.delete_namespace(del, &ctx(), t(T0)).unwrap();
    let up = UpdatePolicy {
        name: "work".into(),
        policy: work_policy(),
        dry_run: false,
    };
    s.update_namespace_policy(up, &ctx(), t(T0)).unwrap();

    save(
        &mut s,
        1,
        json!({
            "summary": "Maya prefers window seats", "fullText": "Maya prefers window seats on long trains",
            "tags": ["travel", "trips/2024"], "entities": ["Maya Smith", "Lisbon"],
            "topics": ["Seats"], "emotions": ["happy"],
        }),
        true,
        T0 + MIN,
    );
    save(
        &mut s,
        2,
        json!({ "summary": "Maya prefers window seats on trains", "entities": ["Maya Smith"], "parentId": mid(1) }),
        true,
        T0 + 2 * MIN,
    );
    let mut sup: SupersedeMemory =
        serde_json::from_value(json!({ "summary": "Maya now prefers aisle seats" })).unwrap();
    sup.id = mid(1);
    s.supersede_memory(sup, mid(3), None, &ctx(), t(T0 + 3 * MIN))
        .unwrap();
    save(
        &mut s,
        4,
        json!({ "summary": "Quarterly budget review" }),
        false,
        T0 + 4 * MIN,
    );
    for _ in 0..3 {
        s.fail_embedding(mid(4));
    }
    let import = |id: MemoryId, phase: &str, extra: serde_json::Value| {
        let mut v = json!({ "type": "memory", "id": id, "namespace": "default",
            "createdAt": t(T0 - 400 * 24 * 60 * MIN).to_rfc3339().unwrap(), "phase": phase });
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        serde_json::from_value::<TransferRecord>(v).unwrap()
    };
    let rec = import(
        mid(5),
        "summary",
        json!({ "summary": "Old trip notes", "isPermastore": true, "stability": 2000.0 }),
    );
    s.import_record(rec, OnConflict::Error, t(T0 + 5 * MIN))
        .unwrap();
    let rec = import(mid(6), "ghost", json!({}));
    s.import_record(rec, OnConflict::Error, t(T0 + 6 * MIN))
        .unwrap();
    let key_ctx = WriteContext {
        saved_by: "api_key:key1".into(),
        client: None,
    };
    let req: StoreMemory = serde_json::from_value(json!({
        "summary": "Launch the rocket engine test", "fullText": "Engine test on Friday at the launch site",
        "namespace": "work", "createdAt": t(T0 - 60 * MIN).to_rfc3339().unwrap(),
    }))
    .unwrap();
    let v = FakeProvider::new().vector(&req.summary);
    s.store_memory(req, mid(7), Some(v), &key_ctx, t(T0 + 7 * MIN))
        .unwrap();

    let hits = [
        AccessHit {
            id: mid(1),
            kind: AccessKind::Direct,
        },
        AccessHit {
            id: mid(2),
            kind: AccessKind::Associative,
        },
    ];
    s.record_access(&hits, t(T0 + 20 * MIN));
    let mut r: ReinforceMemory = serde_json::from_value(json!({ "quality": 4 })).unwrap();
    r.id = mid(2);
    s.reinforce_memory(r, &ctx(), t(T0 + 21 * MIN)).unwrap();

    // An entry whose answer carries memory text (a duplicate report), and one that does not.
    s.run_idempotent("save-8", [8; 32], t(T0 + 30 * MIN), |s: &mut Store| {
        let req: StoreMemory = serde_json::from_value(
            json!({ "summary": "Maya prefers window seats", "checkDuplicates": true }),
        )
        .unwrap();
        let v = FakeProvider::new().vector(&req.summary);
        let r = s.store_memory(req, mid(8), Some(v), &ctx(), t(T0 + 30 * MIN))?;
        let ids = r
            .near_duplicates
            .iter()
            .flat_map(|d| d.matches.iter().map(|m| m.id))
            .collect();
        Ok((r, ids))
    })
    .unwrap();
    s.run_idempotent("forget-9", [9; 32], t(T0 + 31 * MIN), |s: &mut Store| {
        let req = ForgetMemory {
            id: mid(9),
            namespace: None,
        };
        Ok((s.forget_memory(req, &ctx(), t(T0 + 31 * MIN))?, Vec::new()))
    })
    .unwrap();
    // A forget that removed a memory, so the forget counter is in use.
    save(
        &mut s,
        10,
        json!({ "summary": "A note to forget" }),
        false,
        T0 + 40 * MIN,
    );
    let req = ForgetMemory {
        id: mid(10),
        namespace: None,
    };
    s.forget_memory(req, &ctx(), t(T0 + 41 * MIN)).unwrap();
    s
}

/// Writes the three fixtures. Run only for a new content format, into a new directory:
/// `cargo nextest run -p knoggin-crypto --run-ignored only write_v1_fixtures`.
#[test]
#[ignore = "writes the frozen fixtures; they must never be regenerated"]
fn write_v1_fixtures() {
    let full = full();
    assert_eq!(
        edge_types(&full),
        EdgeType::ALL.into_iter().collect(),
        "every edge type"
    );
    for ((file, store, (generation, sequence)), s) in
        [(EMPTY, new_store(1)), (SMALL, small()), (FULL, full)]
    {
        assert_eq!(s.id(), StoreId::from_bytes(store));
        let bytes = seal_snapshot(
            s.id(),
            generation,
            sequence,
            SEALED_MS,
            1,
            s.encode_as(WRITTEN_BY),
        );
        std::fs::create_dir_all(dir()).unwrap();
        let mut f =
            std::fs::File::create_new(dir().join(file)).expect("a fixture is never overwritten");
        std::io::Write::write_all(&mut f, &bytes).unwrap();
    }
}
