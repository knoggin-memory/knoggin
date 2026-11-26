// SPDX-License-Identifier: AGPL-3.0-only
//! Knoggin's ranking equals recalld's: the fixture's store, rebuilt with
//! `StoreBuilder`, answers each of the 40 recorded recalls with recalld's IDs in
//! recalld's order (spec tab "Testing, CI and releases", "Making the ranking fixture").
//!
//! Two allowances, and only these: adjacent hits whose final scores are closer than
//! 0.0005 may come in either order, and closer than 0.02 for a query with `entities`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::collections::BTreeMap;

use knoggin_core::embed::ModelIdentity;
use knoggin_core::model::{
    AccessKind, Edge, EdgeType, EmbeddingState, Labels, Memory, NamespacePolicy, Phase, Tag,
};
use knoggin_core::ops::{Detail, FullHit, Recall, RecallResponse, TimeValue};
use knoggin_core::testing::StoreBuilder;
use knoggin_core::{MemoryId, NamespaceId, Store, Timestamp};
use serde::Deserialize;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/fixtures/ranking/fixture.json"
);
/// Any query: adjacent hits this close may swap.
const NEAR_TIE: f32 = 0.0005;
/// A query with `entities`: adjacent hits this close may swap.
const NEAR_TIE_ENTITIES: f32 = 0.02;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    format: u32,
    namespaces: Vec<FixtureNamespace>,
    memories: Vec<FixtureMemory>,
    edges: Vec<FixtureEdge>,
    queries: Vec<FixtureQuery>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureNamespace {
    name: String,
    embedding_dim: u32,
    initial_stability: f32,
    default_difficulty: f32,
    permastore_threshold: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureMemory {
    id: MemoryId,
    namespace: String,
    created_at_ms: i64,
    summary: String,
    full_text: Option<String>,
    tags: Vec<String>,
    entities: Vec<String>,
    topics: Vec<String>,
    emotions: Vec<String>,
    phase: Phase,
    stability: f32,
    difficulty: f32,
    is_permastore: bool,
    access_count: u64,
    decay_anchor_at_ms: i64,
    vector: Vec<f32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureEdge {
    source: MemoryId,
    target: MemoryId,
    #[serde(rename = "type")]
    edge_type: EdgeType,
    weight: f32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureQuery {
    name: String,
    request: FixtureRequest,
    at_ms: i64,
    vector: Vec<f32>,
    expected_ids: Vec<MemoryId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureRequest {
    query: String,
    namespace: String,
    limit: u32,
    depth: u8,
    #[serde(default)]
    entities: Vec<String>,
    time_range_start: Option<i64>,
    time_range_end: Option<i64>,
}

fn load() -> Fixture {
    let text = std::fs::read_to_string(FIXTURE).expect("read the ranking fixture");
    let fixture: Fixture = serde_json::from_str(&text).expect("parse the ranking fixture");
    assert_eq!(fixture.format, 1);
    fixture
}

/// The fixture's store, exactly as recalld held it.
fn build(fixture: &Fixture) -> Store {
    let dim = fixture.namespaces[0].embedding_dim;
    let model = ModelIdentity {
        id: format!("recalld-embed-stub-{dim}"),
        dimensions: dim,
    };
    let first = fixture
        .memories
        .iter()
        .map(|m| m.created_at_ms)
        .min()
        .unwrap();
    let mut builder = StoreBuilder::new(model, Timestamp::from_millis(first));
    let mut namespaces: BTreeMap<&str, NamespaceId> = BTreeMap::new();
    for ns in &fixture.namespaces {
        let policy = NamespacePolicy {
            initial_stability: ns.initial_stability,
            default_difficulty: ns.default_difficulty,
            permastore_threshold: ns.permastore_threshold,
            ..NamespacePolicy::default()
        };
        let id = builder.namespace(&ns.name, policy, Timestamp::from_millis(first));
        namespaces.insert(&ns.name, id);
    }
    let mut created = BTreeMap::new();
    for m in &fixture.memories {
        let created_at = Timestamp::from_millis(m.created_at_ms);
        created.insert(m.id, created_at);
        let memory = Memory {
            id: m.id,
            namespace: namespaces[m.namespace.as_str()],
            created_at,
            summary: m.summary.clone(),
            full_text: m.full_text.clone(),
            tags: m
                .tags
                .iter()
                .map(|t| Tag::new(t.as_str()).unwrap())
                .collect(),
            labels: Labels {
                entities: m.entities.clone(),
                topics: m.topics.clone(),
                emotions: m.emotions.clone(),
            },
            phase: m.phase,
            stability: m.stability,
            difficulty: m.difficulty,
            is_permastore: m.is_permastore,
            decay_anchor_at: Timestamp::from_millis(m.decay_anchor_at_ms),
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: created_at,
            access_count: m.access_count,
            access_history: Vec::new(),
            embedding: EmbeddingState::Ready,
            embed_failures: 0,
            saved_by: "import".into(),
            client: None,
        };
        builder.memory(memory, Some(&m.vector));
    }
    for e in &fixture.edges {
        builder.edge(Edge {
            source: e.source,
            target: e.target,
            edge_type: e.edge_type,
            weight: e.weight,
            auto_created: matches!(
                e.edge_type,
                EdgeType::Associative | EdgeType::Entity | EdgeType::Temporal
            ),
            created_at: created[&e.source],
        });
    }
    builder.build()
}

fn request(q: &FixtureRequest, limit: u32) -> Recall {
    Recall {
        query: q.query.clone(),
        namespace: Some(q.namespace.clone()),
        limit,
        full_text_count: None,
        tags: Vec::new(),
        entities: q.entities.clone(),
        topics: Vec::new(),
        emotions: Vec::new(),
        min_strength: None,
        depth: q.depth,
        time_range_start: q.time_range_start.map(TimeValue::Millis),
        time_range_end: q.time_range_end.map(TimeValue::Millis),
        detail: Detail::Full,
        record_access: false,
    }
}

fn run(store: &Store, q: &FixtureQuery, limit: u32) -> Vec<FullHit> {
    let result = store
        .recall(
            &request(&q.request, limit),
            Some(&q.vector),
            false,
            Timestamp::from_millis(q.at_ms),
        )
        .expect("recall");
    assert!(
        result
            .hits
            .iter()
            .all(|h| matches!(h.kind, AccessKind::Direct | AccessKind::Associative))
    );
    match result.response {
        RecallResponse::Full(full) => full.memories,
        RecallResponse::Compact(_) => panic!("asked for the full shape"),
    }
}

/// How one query compared: `Ok(swaps)` lists the near-tie pairs that came in the
/// other order, as `(rank, gap)`; `Err` says where the order differs beyond them.
fn compare(store: &Store, q: &FixtureQuery) -> Result<Vec<(usize, f32)>, String> {
    let tolerance = if q.request.entities.is_empty() {
        NEAR_TIE
    } else {
        NEAR_TIE_ENTITIES
    };
    let hits = run(store, q, q.request.limit);
    let got: Vec<MemoryId> = hits.iter().map(|h| h.memory.id).collect();
    let want = &q.expected_ids;
    let describe = |i: usize| {
        let line = |h: &FullHit| format!("{} {:.6}", h.memory.id, h.score);
        format!(
            "{}: rank {i}: got {:?}, expected {:?}; got around it [{}]",
            q.name,
            got.get(i).map(ToString::to_string),
            want.get(i).map(ToString::to_string),
            hits.iter()
                .skip(i.saturating_sub(2))
                .take(5)
                .map(line)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    if got.len() != want.len() {
        return Err(format!(
            "{}: {} hits, expected {}",
            q.name,
            got.len(),
            want.len()
        ));
    }
    let mut swaps = Vec::new();
    let mut i = 0;
    while i < got.len() {
        if got[i] == want[i] {
            i += 1;
            continue;
        }
        // The two allowances of the spec: adjacent hits in Knoggin's own result whose
        // scores are closer than the tolerance may come in either order.
        if i + 1 < got.len() && got[i] == want[i + 1] && got[i + 1] == want[i] {
            let gap = (hits[i].score - hits[i + 1].score).abs();
            if gap < tolerance {
                swaps.push((i, gap));
                i += 2;
                continue;
            }
        }
        return Err(describe(i));
    }
    Ok(swaps)
}

#[test]
fn knoggin_ranks_the_fixture_queries_as_recalld_did() {
    let fixture = load();
    assert_eq!(fixture.queries.len(), 40);
    let store = build(&fixture);
    let (mut exact, mut allowed, mut failures) = (0, 0, Vec::new());
    for q in &fixture.queries {
        match compare(&store, q) {
            Ok(swaps) if swaps.is_empty() => {
                exact += 1;
                println!("{} ok: {} hits, exact", q.name, q.expected_ids.len());
            }
            Ok(swaps) => {
                allowed += 1;
                let pairs: Vec<String> = swaps
                    .iter()
                    .map(|(rank, gap)| format!("ranks {rank}/{} gap {gap:.6}", rank + 1))
                    .collect();
                println!(
                    "{} ok: {} hits, near-tie swaps: {}",
                    q.name,
                    q.expected_ids.len(),
                    pairs.join("; ")
                );
            }
            Err(why) => {
                println!("{why}");
                failures.push(q.name.clone());
            }
        }
    }
    println!(
        "{exact} exact, {allowed} within the near-tie allowance, {} different",
        failures.len()
    );
    assert!(failures.is_empty(), "queries that differ: {failures:?}");
}
