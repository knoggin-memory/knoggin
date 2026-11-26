// SPDX-License-Identifier: AGPL-3.0-only
//! Recall pipeline tests. The supersedes tests are ported from recalld
//! `src/search/pipeline.rs` (`resolve_supersedes`), on a real store instead of stubs.

use super::*;
use crate::embed::EmbeddingProvider;
use crate::model::{Edge, Namespace, NamespacePolicy};
use crate::ops::TimeValue;
use crate::testing::{FakeProvider, StoreBuilder, memory};

const NS: NamespaceId = NamespaceId::new(1);
const OTHER: NamespaceId = NamespaceId::new(2);
const T0: i64 = 1_700_000_000_000;

fn mid(n: u8) -> MemoryId {
    let mut b = [0; 16];
    b[15] = n;
    MemoryId::from_bytes(b)
}

fn t(ms: i64) -> Timestamp {
    Timestamp::from_millis(ms)
}

fn vector(text: &str) -> Vec<f32> {
    FakeProvider::new().vector(text)
}

fn edge(s: u8, t_: u8, edge_type: EdgeType) -> Edge {
    Edge {
        source: mid(s),
        target: mid(t_),
        edge_type,
        weight: 1.0,
        auto_created: false,
        created_at: t(T0),
    }
}

/// A store with namespaces 1 (`default`) and 2 (`other`); memory `n` is saved at
/// `T0 + n` seconds with the summary given and a vector of it.
fn store(memories: &[(u8, NamespaceId, &str)], edges: &[Edge]) -> Store {
    let mut b = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0));
    b.namespace("other", NamespacePolicy::default(), t(T0));
    for &(n, ns, summary) in memories {
        let m = memory(mid(n), ns, summary, t(T0 + i64::from(n) * 1_000));
        b.memory(m, Some(&vector(summary)));
    }
    for e in edges {
        b.edge(*e);
    }
    b.build()
}

fn recall(query: &str) -> Recall {
    serde_json::from_value(serde_json::json!({ "query": query })).unwrap()
}

fn full(mut req: Recall) -> Recall {
    req.detail = Detail::Full;
    req
}

fn run(s: &Store, req: &Recall) -> RecallResult {
    s.recall(req, Some(&vector(&req.query)), true, t(T0 + 60_000))
        .unwrap()
}

fn ids(result: &RecallResult) -> Vec<MemoryId> {
    result.trace.returned.clone()
}

fn full_hits(result: &RecallResult) -> &[FullHit] {
    match &result.response {
        RecallResponse::Full(f) => &f.memories,
        RecallResponse::Compact(_) => panic!("full shape expected"),
    }
}

fn filter() -> HardFilter {
    HardFilter {
        namespace: NS,
        min_strength: None,
        tags: Vec::new(),
        entities: Vec::new(),
        topics: Vec::new(),
        emotions: Vec::new(),
    }
}

fn candidate(n: u8, score: f32) -> Candidate {
    Candidate {
        score,
        relevance: Some(score),
        ..Candidate::new(mid(n))
    }
}

fn resolve(s: &Store, candidates: &mut Vec<Candidate>, filter: &HardFilter) {
    s.resolve_supersedes(candidates, filter, &[], t(T0 + 60_000));
}

fn cand_ids(candidates: &[Candidate]) -> Vec<MemoryId> {
    candidates.iter().map(|c| c.id).collect()
}

// ── Supersedes (ported) ───────────────────────────────────────────────

#[test]
fn substitutes_an_eligible_replacement_for_the_superseded_memory() {
    let s = store(
        &[(1, NS, "old"), (2, NS, "new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    let mut c = vec![candidate(1, 0.8)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(2)]);
    assert!(c[0].score >= 0.8);
    assert_eq!(c[0].replaced, Some(mid(1)));
}

#[test]
fn replacement_already_among_the_candidates_appears_exactly_once() {
    let s = store(
        &[(1, NS, "old"), (2, NS, "new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    let mut c = vec![candidate(1, 0.8), candidate(2, 0.9)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(2)]);
    assert_eq!(c[0].score, 0.9, "its own better score stays");
}

#[test]
fn ghost_replacement_is_kept_out() {
    let mut s = store(
        &[(1, NS, "old"), (2, NS, "new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    s.fade_to_ghost(mid(2), t(T0));
    let mut c = vec![candidate(1, 0.8)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(1)]);
}

#[test]
fn replacement_from_another_namespace_is_not_leaked() {
    let s = store(
        &[(1, NS, "old"), (2, OTHER, "new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    let mut c = vec![candidate(1, 0.8)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(1)]);
}

#[test]
fn replacement_failing_metadata_filters_is_not_substituted() {
    let mut s = store(
        &[(1, NS, "old"), (2, NS, "new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    let tag = Tag::new("project/x").unwrap();
    s.update_memory(mid(1), |m| m.tags = vec![tag.clone()]);
    let tagged = HardFilter {
        tags: vec![tag],
        ..filter()
    };
    let mut c = vec![candidate(1, 0.8)];
    resolve(&s, &mut c, &tagged);
    assert_eq!(cand_ids(&c), [mid(1)], "required tag missing");

    // The replacement has faded below the minimum strength; the original has not.
    s.update_memory(mid(2), |m| {
        m.stability = 0.01;
        m.decay_anchor_at = t(T0 - 400 * MILLIS_PER_DAY);
    });
    let strong = HardFilter {
        min_strength: Some(0.5),
        ..filter()
    };
    let mut c = vec![candidate(1, 0.8)];
    resolve(&s, &mut c, &strong);
    assert_eq!(cand_ids(&c), [mid(1)], "below minimum strength");
}

#[test]
fn many_superseded_memories_share_one_replacement() {
    let s = store(
        &[(1, NS, "a"), (2, NS, "b"), (3, NS, "new")],
        &[
            edge(3, 1, EdgeType::Supersedes),
            edge(3, 2, EdgeType::Supersedes),
        ],
    );
    let mut c = vec![candidate(1, 0.5), candidate(2, 0.85)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(3)]);
    assert_eq!(c[0].score, 0.85);
    assert_eq!(c[0].replaced, Some(mid(2)), "the best of what it replaces");
}

#[test]
fn whole_chain_collapses_to_its_newest_version() {
    // a <- b <- c
    let s = store(
        &[(1, NS, "a"), (2, NS, "b"), (3, NS, "c")],
        &[
            edge(2, 1, EdgeType::Supersedes),
            edge(3, 2, EdgeType::Supersedes),
        ],
    );
    let mut c = vec![candidate(1, 0.9), candidate(2, 0.7), candidate(3, 0.4)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(3)]);
    assert!(c[0].score >= 0.9);
}

#[test]
fn chain_with_unreturnable_newest_falls_back_to_the_version_before_it() {
    let mut s = store(
        &[(1, NS, "a"), (2, NS, "b"), (3, NS, "c")],
        &[
            edge(2, 1, EdgeType::Supersedes),
            edge(3, 2, EdgeType::Supersedes),
        ],
    );
    s.fade_to_ghost(mid(3), t(T0));
    let mut c = vec![candidate(1, 0.9)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(2)]);
    assert_eq!(c[0].score, 0.9);

    let mut c = vec![candidate(1, 0.9), candidate(2, 0.6)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(2)]);
    assert_eq!(c[0].score, 0.9);
}

#[test]
fn of_several_successors_the_newest_wins_and_cycles_end() {
    // 1 is superseded by 2 and by 3; 3 is newer. 4 and 5 supersede each other.
    let s = store(
        &[
            (1, NS, "a"),
            (2, NS, "b"),
            (3, NS, "c"),
            (4, NS, "d"),
            (5, NS, "e"),
        ],
        &[
            edge(2, 1, EdgeType::Supersedes),
            edge(3, 1, EdgeType::Supersedes),
            edge(4, 5, EdgeType::Supersedes),
            edge(5, 4, EdgeType::Supersedes),
        ],
    );
    let mut c = vec![candidate(1, 0.5)];
    resolve(&s, &mut c, &filter());
    assert_eq!(cand_ids(&c), [mid(3)]);
    assert_eq!(s.successors(mid(4)), [mid(5)]);
    assert!(s.successors(mid(3)).is_empty());
}

// ── Formulas ──────────────────────────────────────────────────────────

#[test]
fn fusion_and_scores_follow_recalld() {
    assert_eq!(fuse(None, None), None);
    assert_eq!(fuse(Some(0.5), None), Some(0.5));
    let both = fuse(Some(0.5), Some(2.0)).unwrap();
    assert!((both - (0.5 + 0.05 * (1.0 - (-1.0_f32).exp()))).abs() < 1e-7);
    let text = fuse(None, Some(2.0)).unwrap();
    assert!((text - 0.30 * (1.0 - (-0.6_f32).exp())).abs() < 1e-7);

    let direct = Candidate {
        relevance: Some(0.5),
        text: Some(2.0),
        entity_score: 0.5,
        ..Candidate::new(mid(1))
    };
    let signal = 1.0 - (-1.0_f32).exp();
    assert!((composite_score(&direct) - (0.45 + 0.025 + 0.05 * signal)).abs() < 1e-7);
    let weak_text = Candidate {
        text: Some(0.5),
        ..direct.clone()
    };
    assert!(
        (composite_score(&weak_text) - 0.475).abs() < 1e-7,
        "no signal at 0.5"
    );
    let graph = Candidate {
        activation: Some(0.4),
        entity_score: 0.5,
        ..Candidate::new(mid(1))
    };
    assert!((composite_score(&graph) - (0.28 + 0.15)).abs() < 1e-7);
    let entity = Candidate {
        entity_recall: 2.0,
        entity_score: 0.5,
        effective_r: 0.9,
        ..Candidate::new(mid(1))
    };
    assert!((composite_score(&entity) - (0.8 + 0.1 + 0.09)).abs() < 1e-6);
}

#[test]
fn time_boost_peaks_at_the_middle_with_a_one_day_floor() {
    let (start, end) = (t(0), t(10 * MILLIS_PER_DAY));
    assert_eq!(time_boost(t(5 * MILLIS_PER_DAY), start, end), 1.5);
    let edge = time_boost(t(10 * MILLIS_PER_DAY), start, end);
    assert!((edge - (1.0 + 0.5 * (-0.5_f32).exp())).abs() < 1e-6);
    // A one-minute range still has a sigma of one day.
    let narrow = time_boost(t(MILLIS_PER_DAY), t(0), t(60_000));
    assert!(
        (narrow - (1.0 + 0.5 * (-0.5_f32).exp())).abs() < 1e-3,
        "{narrow}"
    );
}

// ── The whole pipeline ───────────────────────────────────────────────

#[test]
fn every_limit_is_checked() {
    let s = store(&[], &[]);
    let field = |req: Recall| s.recall(&req, None, false, t(T0)).unwrap_err().field();
    assert_eq!(field(recall("  ")), Some("query"));
    assert_eq!(field(recall(&"x".repeat(4_097))), Some("query"));
    let with = |patch: serde_json::Value| {
        let mut v = serde_json::json!({ "query": "q" });
        v.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        serde_json::from_value::<Recall>(v).unwrap()
    };
    assert_eq!(
        field(with(serde_json::json!({ "limit": 0 }))),
        Some("limit")
    );
    assert_eq!(
        field(with(serde_json::json!({ "limit": 101 }))),
        Some("limit")
    );
    assert_eq!(
        field(with(serde_json::json!({ "limit": 3, "fullTextCount": 4 }))),
        Some("fullTextCount")
    );
    assert_eq!(
        field(with(serde_json::json!({ "tags": ["Bad Tag"] }))),
        Some("tags")
    );
    assert_eq!(
        field(with(serde_json::json!({ "minStrength": 1.5 }))),
        Some("minStrength")
    );
    assert_eq!(
        field(with(serde_json::json!({ "depth": 4 }))),
        Some("depth")
    );
    assert_eq!(
        field(with(serde_json::json!({ "timeRangeEnd": 5 }))),
        Some("timeRangeStart")
    );
    assert_eq!(
        field(with(
            serde_json::json!({ "timeRangeStart": -5, "timeRangeEnd": 5 })
        )),
        Some("timeRangeStart")
    );
    let unknown = s
        .recall(
            &with(serde_json::json!({ "namespace": "nope" })),
            None,
            false,
            t(T0),
        )
        .unwrap_err();
    assert_eq!(unknown.code(), crate::error::ErrorCode::NotFound);
    assert!(
        s.recall(
            &with(serde_json::json!({ "namespace": "OTHER" })),
            None,
            false,
            t(T0)
        )
        .is_ok(),
        "names match ignoring ASCII case"
    );
}

#[test]
fn without_a_vector_keyword_search_still_answers_and_says_so() {
    let s = store(
        &[
            (1, NS, "window seats on planes"),
            (2, NS, "quarterly revenue"),
        ],
        &[],
    );
    let r = s.recall(&recall("window"), None, false, t(T0)).unwrap();
    assert_eq!(ids(&r), [mid(1)]);
    let RecallResponse::Compact(c) = &r.response else {
        panic!("compact by default");
    };
    assert_eq!(c.degraded, Some(Degraded::ModelUnavailable));
    // A vector of the wrong size counts as none.
    let r = s
        .recall(&recall("window"), Some(&[1.0]), false, t(T0))
        .unwrap();
    assert_eq!(ids(&r), [mid(1)]);
    let with = run(&s, &recall("window"));
    let RecallResponse::Compact(c) = &with.response else {
        panic!()
    };
    assert_eq!(c.degraded, None);
}

#[test]
fn compact_hits_carry_the_date_and_full_text_only_at_the_top() {
    let mut s = store(
        &[
            (1, NS, "alpha beta"),
            (2, NS, "alpha gamma"),
            (3, NS, "alpha delta"),
        ],
        &[],
    );
    for n in 1..=3 {
        s.update_memory(mid(n), |m| m.full_text = Some(format!("long {n}")));
    }
    let mut req = recall("alpha");
    req.full_text_count = Some(1);
    let r = run(&s, &req);
    let RecallResponse::Compact(c) = &r.response else {
        panic!()
    };
    assert_eq!(c.count, 3);
    assert!(c.memories[0].full_text.is_some());
    assert!(c.memories[1..].iter().all(|h| h.full_text.is_none()));
    assert_eq!(c.memories[0].date.len(), "Tue 2023-11-14".len());
    assert!(r.hits.iter().all(|h| h.kind == AccessKind::Direct));
}

#[test]
fn full_hits_list_edges_to_other_hits_and_explain_only_when_asked() {
    let s = store(
        &[(1, NS, "alpha one"), (2, NS, "alpha two"), (3, NS, "zulu")],
        &[edge(1, 2, EdgeType::Entity), edge(1, 3, EdgeType::Temporal)],
    );
    let r = run(&s, &full(recall("alpha")));
    let hits = full_hits(&r);
    let one = hits.iter().find(|h| h.memory.id == mid(1)).unwrap();
    // Only edges to other hits on the page are listed.
    let page: BTreeSet<MemoryId> = hits.iter().map(|h| h.memory.id).collect();
    assert!(one.related.iter().all(|r| page.contains(&r.id)));
    assert!(
        one.related
            .iter()
            .any(|r| r.id == mid(2) && r.edge_type == EdgeType::Entity)
    );
    let e = one.explanation.unwrap();
    assert!(e.vector.is_some() && e.text.is_some() && e.activation.is_none());
    let plain = s
        .recall(&full(recall("alpha")), Some(&vector("alpha")), false, t(T0))
        .unwrap();
    assert!(full_hits(&plain).iter().all(|h| h.explanation.is_none()));
}

#[test]
fn a_graph_hit_without_relevance_is_reached_through_its_seed() {
    // Only 1 has a vector; 2 is pending, so only the graph can find it.
    let mut b = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0));
    b.memory(memory(mid(1), NS, "alpha", t(T0)), Some(&vector("alpha")));
    b.memory(memory(mid(2), NS, "zulu", t(T0)), None);
    b.edge(edge(1, 2, EdgeType::Associative));
    let s = b.build();
    let mut req = full(recall("alpha"));
    req.depth = 1;
    let r = run(&s, &req);
    assert_eq!(ids(&r), [mid(1), mid(2)]);
    let hits = full_hits(&r);
    let e = hits[1].explanation.unwrap();
    assert_eq!(
        (e.vector, e.text, e.via, e.via_edge_type, e.hops),
        (
            None,
            None,
            Some(mid(1)),
            Some(EdgeType::Associative),
            Some(1)
        )
    );
    assert!(e.activation.is_some());
    assert_eq!(r.hits[1].kind, AccessKind::Associative);
    assert_eq!(r.trace.seeds.len(), 1);
    assert_eq!(r.trace.spreads[0].from, mid(1));
    assert_eq!(r.trace.spreads[0].order, 0);
}

#[test]
fn entities_filter_case_insensitively_and_feed_the_entity_channel() {
    let mut s = store(&[(1, NS, "alpha"), (2, NS, "beta"), (3, NS, "gamma")], &[]);
    for n in [1, 2] {
        s.update_memory(mid(n), |m| m.labels.entities = vec!["Maya Chen".into()]);
    }
    let mut req = full(recall("alpha"));
    req.entities = vec!["MAYA CHEN".into()];
    let r = run(&s, &req);
    let got = ids(&r);
    assert!(got.contains(&mid(1)) && !got.contains(&mid(3)), "{got:?}");
    assert!(
        full_hits(&r)
            .iter()
            .all(|h| h.memory.entities == ["Maya Chen"])
    );
}

#[test]
fn entity_only_candidates_are_scored_by_shared_keys() {
    // Without a vector and with a query that matches no text, only the entity channel
    // finds anything.
    let mut s = store(&[(1, NS, "alpha"), (2, NS, "beta")], &[]);
    s.update_memory(mid(1), |m| m.labels.entities = vec!["Maya Chen".into()]);
    let mut req = full(recall("zzz"));
    req.entities = vec!["Maya Chen".into()];
    let r = s.recall(&req, None, true, t(T0)).unwrap();
    assert_eq!(ids(&r), [mid(1)]);
    let hit = &full_hits(&r)[0];
    // Two keys shared ("maya chen" and "chen") over one query entity: 2.0.
    let expected = 0.40 * 2.0 + 0.20 * 1.0 + 0.10 * hit.memory.effective_strength;
    assert!(
        (hit.score - expected).abs() < 1e-6,
        "{} vs {expected}",
        hit.score
    );
}

#[test]
fn retrieval_induced_forgetting_lowers_an_entity_only_candidate() {
    // Three entity-only candidates with equal entity scores; 1 and 2 share an edge, so
    // each suppresses the other, and 3, otherwise last by ID, comes first.
    let mut s = store(
        &[(1, NS, "alpha"), (2, NS, "beta"), (3, NS, "gamma")],
        &[edge(1, 2, EdgeType::Associative)],
    );
    for n in 1..=3 {
        s.update_memory(mid(n), |m| m.labels.entities = vec!["Maya".into()]);
    }
    let mut req = full(recall("zzz"));
    req.entities = vec!["Maya".into()];
    let now = t(T0 + 60_000);
    let r = s.recall(&req, None, true, now).unwrap();
    assert_eq!(ids(&r), [mid(3), mid(1), mid(2)]);
    assert_eq!(r.trace.suppressed, [mid(1), mid(2)]);
    let hits = full_hits(&r);
    let strength = |n: u8| s.raw_strength(s.memory(mid(n)).unwrap(), now);
    let multiplier = crate::rif::plasticity_multiplier(0.3 * strength(2));
    assert!(multiplier < 1.0);
    // The suppressed score uses effective strength times the multiplier.
    let effective = s.strengths(s.memory(mid(1)).unwrap(), now).1;
    let expected = 0.40 + 0.20 + 0.10 * effective * multiplier;
    assert!(
        (hits[1].score - expected).abs() < 1e-6,
        "{} vs {expected}",
        hits[1].score
    );
    assert!(hits[0].score - hits[1].score > 0.01);
}

#[test]
fn a_superseded_hit_is_replaced_by_its_successor() {
    let s = store(
        &[(1, NS, "alpha old"), (2, NS, "zulu new")],
        &[edge(2, 1, EdgeType::Supersedes)],
    );
    let r = s.recall(&full(recall("alpha")), None, true, t(T0)).unwrap();
    assert_eq!(ids(&r), [mid(2)]);
    let hit = &full_hits(&r)[0];
    assert_eq!(hit.explanation.unwrap().replaced, Some(mid(1)));
    assert!(hit.score > 0.0);
}

#[test]
fn a_negative_cosine_without_text_is_not_returned() {
    let mut b = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0));
    let q = vector("alpha");
    let opposite: Vec<f32> = q.iter().map(|x| -x).collect();
    b.memory(memory(mid(1), NS, "zulu", t(T0)), Some(&opposite));
    b.memory(memory(mid(2), NS, "yankee", t(T0)), Some(&q));
    let s = b.build();
    assert_eq!(ids(&run(&s, &recall("alpha"))), [mid(2)]);
}

#[test]
fn a_time_range_boosts_memories_near_its_middle() {
    let mut s = store(&[(1, NS, "alpha"), (2, NS, "alpha")], &[]);
    // Same text and vector; 1 was saved ten days before the range.
    s.update_memory(mid(1), |m| m.created_at = t(T0 - 10 * MILLIS_PER_DAY));
    let mut req = full(recall("alpha"));
    req.time_range_start = Some(TimeValue::Millis(T0));
    req.time_range_end = Some(TimeValue::Millis(T0 + 4_000));
    let r = run(&s, &req);
    let hits = full_hits(&r);
    assert_eq!(hits[0].memory.id, mid(2));
    assert!(hits[0].score > hits[1].score * 1.4);
}

#[test]
fn ghosts_and_other_namespaces_are_never_returned() {
    let mut s = store(
        &[(1, NS, "alpha"), (2, OTHER, "alpha"), (3, NS, "alpha beta")],
        &[],
    );
    s.fade_to_ghost(mid(3), t(T0));
    assert_eq!(ids(&run(&s, &recall("alpha"))), [mid(1)]);
}

#[test]
fn a_namespace_can_be_named_and_debug_hides_filters() {
    let mut s = store(&[], &[]);
    s.insert_namespace(Namespace {
        id: NamespaceId::new(3),
        name: "Marker".into(),
        created_at: t(T0),
        policy: NamespacePolicy::default(),
    });
    assert_eq!(s.resolve_namespace(Some("marker")).unwrap().id.get(), 3);
    assert_eq!(s.resolve_namespace(None).unwrap().id, NS);
    let f = HardFilter {
        entities: vec!["marker-secret".into()],
        ..filter()
    };
    assert!(!format!("{f:?}").contains("marker"));
}
