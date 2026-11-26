// SPDX-License-Identifier: AGPL-3.0-only
//! Tests of the reads other than recall.

use super::*;
use crate::embed::EmbeddingProvider;
use crate::error::ErrorCode;
use crate::id::NamespaceId;
use crate::model::{AccessEvent, AccessKind, Edge, EdgeType, NamespacePolicy, Tag};
use crate::ops::Degraded;
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

/// Memory `n` is saved at `T0 + n` seconds with the given tags and full text "long n".
fn store(memories: &[(u8, NamespaceId, &str, &[&str])]) -> Store {
    let mut b = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0));
    b.namespace("other", NamespacePolicy::default(), t(T0));
    for &(n, ns, summary, tags) in memories {
        let mut m = memory(mid(n), ns, summary, t(T0 + i64::from(n) * 1_000));
        m.full_text = Some(format!("long {n}"));
        m.tags = tags.iter().map(|t| Tag::new(*t).unwrap()).collect();
        b.memory(m, Some(&vector(summary)));
    }
    b.build()
}

fn json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).unwrap()
}

#[test]
fn get_returns_full_text_checks_the_namespace_and_hides_a_ghost_summary() {
    let mut s = store(&[(1, NS, "alpha", &[]), (2, NS, "beta", &[])]);
    let now = t(T0 + 60_000);
    let event = AccessEvent {
        at: now,
        kind: AccessKind::Direct,
    };
    s.update_memory(mid(1), |m| m.access_history = vec![event]);
    let get = |v| s.get_memory(&json(v), now);
    let view = get(serde_json::json!({ "id": mid(1) })).unwrap();
    assert_eq!(view.full_text.as_deref(), Some("long 1"));
    assert_eq!(view.access_history, None);
    let view = get(serde_json::json!({ "id": mid(1), "includeHistory": true })).unwrap();
    assert_eq!(view.access_history, Some(vec![event]));
    assert!(get(serde_json::json!({ "id": mid(1), "namespace": "default" })).is_ok());
    for bad in [
        serde_json::json!({ "id": mid(1), "namespace": "other" }),
        serde_json::json!({ "id": mid(1), "namespace": "missing" }),
        serde_json::json!({ "id": mid(9) }),
    ] {
        assert_eq!(get(bad).unwrap_err().code(), ErrorCode::NotFound);
    }
    s.fade_to_ghost(mid(2), now);
    let ghost = s
        .get_memory(&json(serde_json::json!({ "id": mid(2) })), now)
        .unwrap();
    assert_eq!((ghost.summary, ghost.full_text), (None, None));
}

#[test]
fn similar_uses_the_memory_vector_within_its_namespace() {
    let mut s = store(&[
        (1, NS, "window seats on the plane", &[]),
        (2, NS, "window seats please", &[]),
        (3, NS, "quarterly revenue grew", &[]),
        (4, OTHER, "window seats on the plane", &[]),
        (5, NS, "window seats on the plane too", &[]),
    ]);
    let now = t(T0);
    s.fade_to_ghost(mid(5), now);
    let similar = |s: &Store, v: serde_json::Value| {
        let mut req: FindSimilar = json(v);
        req.id = mid(1);
        s.find_similar(&req, now)
    };
    let all = similar(&s, serde_json::json!({})).unwrap();
    let got: Vec<MemoryId> = all.items.iter().map(|h| h.memory.id).collect();
    assert_eq!(
        got,
        [mid(2), mid(3)],
        "not itself, other namespaces or ghosts"
    );
    assert!(all.items[0].score > all.items[1].score);
    assert!(all.items.iter().all(|h| h.memory.full_text.is_none()));
    let cut = all.items[0].score;
    let high = similar(&s, serde_json::json!({ "minScore": cut })).unwrap();
    assert_eq!(high.items.len(), 1);
    let one = similar(&s, serde_json::json!({ "limit": 1 })).unwrap();
    assert_eq!(one.items.len(), 1);
    for (v, field) in [
        (serde_json::json!({ "limit": 0 }), "limit"),
        (serde_json::json!({ "limit": 101 }), "limit"),
        (serde_json::json!({ "minScore": 2.0 }), "minScore"),
    ] {
        assert_eq!(similar(&s, v).unwrap_err().field(), Some(field));
    }
    // No vector yet.
    s.update_memory(mid(1), |m| {
        m.embedding = crate::model::EmbeddingState::Pending
    });
    let err = similar(&s, serde_json::json!({})).unwrap_err();
    assert_eq!(err.code(), ErrorCode::Conflict);
    assert_eq!(err.to_string(), "embedding pending");
}

#[test]
fn list_pages_newest_first_by_an_opaque_cursor() {
    let mut s = store(&[
        (1, NS, "a", &["x"]),
        (2, NS, "b", &["x", "y"]),
        (3, NS, "c", &["x"]),
        (4, OTHER, "d", &["x"]),
        (5, NS, "e", &[]),
    ]);
    let now = t(T0);
    let list = |s: &Store, v| s.list_memories(&json(v), now).unwrap();
    let page = list(&s, serde_json::json!({ "limit": 2 }));
    let got: Vec<_> = page.items.iter().map(|m| m.id).collect();
    assert_eq!(got, [mid(5), mid(3)]);
    assert!(page.items.iter().all(|m| m.full_text.is_none()));
    let cursor = page.next_cursor.unwrap();
    // The page after a memory that no longer exists still starts after it.
    s.remove_memory(mid(3));
    let next = list(&s, serde_json::json!({ "limit": 2, "cursor": cursor }));
    let got: Vec<_> = next.items.iter().map(|m| m.id).collect();
    assert_eq!(got, [mid(2), mid(1)]);
    assert_eq!(next.next_cursor, None);

    let tagged = list(
        &s,
        serde_json::json!({ "tags": ["x", "Y"], "includeFullText": true }),
    );
    assert_eq!(tagged.items.len(), 1);
    assert_eq!(tagged.items[0].full_text.as_deref(), Some("long 2"));
    let window = list(
        &s,
        serde_json::json!({
            "createdAfter": t(T0 + 1_000), "createdBefore": t(T0 + 5_000), "phase": "full"
        }),
    );
    let got: Vec<_> = window.items.iter().map(|m| m.id).collect();
    assert_eq!(got, [mid(2)], "both bounds are strict");
    let other = list(&s, serde_json::json!({ "namespace": "other" }));
    assert_eq!(other.items.len(), 1);

    for (v, field) in [
        (serde_json::json!({ "limit": 0 }), "limit"),
        (serde_json::json!({ "limit": 201 }), "limit"),
        (serde_json::json!({ "cursor": "nonsense" }), "cursor"),
        (serde_json::json!({ "cursor": "1.not-a-uuid" }), "cursor"),
        // Base64url of text that is not a cursor, and of bytes that are not UTF-8.
        (
            serde_json::json!({ "cursor": "MS5ub3QtYS11dWlk" }),
            "cursor",
        ),
        (serde_json::json!({ "cursor": "_w" }), "cursor"),
        (serde_json::json!({ "tags": ["bad tag"] }), "tags"),
    ] {
        let err = s.list_memories(&json(v), now).unwrap_err();
        assert_eq!(err.field(), Some(field));
    }
    let missing = s.list_memories(&json(serde_json::json!({ "namespace": "nope" })), now);
    assert_eq!(missing.unwrap_err().code(), ErrorCode::NotFound);
}

#[test]
fn namespaces_come_by_name_with_counts() {
    let mut b = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0));
    b.namespace("zeta", NamespacePolicy::default(), t(T0));
    let alpha = b.namespace("alpha", NamespacePolicy::default(), t(T0));
    b.memory(memory(mid(1), alpha, "a", t(T0)), None);
    let s = b.build();
    let names: Vec<(String, u64)> = s
        .list_namespaces()
        .into_iter()
        .map(|n| (n.name, n.memory_count))
        .collect();
    assert_eq!(
        names,
        [
            ("alpha".to_owned(), 1),
            ("default".to_owned(), 0),
            ("zeta".to_owned(), 0)
        ]
    );
}

#[test]
fn tags_count_plain_tags_by_count_then_name() {
    let s = store(&[
        (1, NS, "a", &["b-tag", "a-tag", "c-tag"]),
        (2, NS, "b", &["b-tag", "c-tag"]),
        (3, NS, "c", &["c-tag", "other"]),
        (4, OTHER, "d", &["a-tag"]),
    ]);
    let tags = |v| s.list_tags(&json(v)).unwrap();
    let all = tags(serde_json::json!({}));
    let got: Vec<(String, u64)> = all.items.iter().map(|t| (t.tag.clone(), t.count)).collect();
    assert_eq!(
        got,
        [
            ("c-tag".to_owned(), 3),
            ("b-tag".to_owned(), 2),
            ("a-tag".to_owned(), 1),
            ("other".to_owned(), 1)
        ]
    );
    let first = tags(serde_json::json!({ "limit": 2 }));
    assert_eq!(first.items.len(), 2);
    let cursor = first.next_cursor.as_deref().unwrap();
    assert!(
        !cursor.contains("b-tag")
            && cursor
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "a cursor is base64url and does not show the tag: {cursor}"
    );
    let rest = tags(serde_json::json!({ "limit": 2, "cursor": first.next_cursor.unwrap() }));
    let names: Vec<&str> = rest.items.iter().map(|t| t.tag.as_str()).collect();
    assert_eq!(names, ["a-tag", "other"]);
    assert_eq!(rest.next_cursor, None);
    let prefixed = tags(serde_json::json!({ "prefix": "B-" }));
    assert_eq!(prefixed.items.len(), 1);
    assert_eq!(
        s.list_tags(&json(serde_json::json!({ "cursor": "x" })))
            .unwrap_err()
            .field(),
        Some("cursor")
    );
    // "3" alone: base64url of a cursor without its separator.
    assert_eq!(
        s.list_tags(&json(serde_json::json!({ "cursor": "Mw" })))
            .unwrap_err()
            .code(),
        crate::ErrorCode::InvalidArgument
    );
}

#[test]
fn stats_count_what_the_namespace_holds() {
    let mut s = store(&[
        (1, NS, "alpha", &[]),
        (2, NS, "beta gamma", &[]),
        (3, NS, "c", &[]),
        (4, OTHER, "d", &[]),
    ]);
    let edge = |a: u8, b: u8| Edge {
        source: mid(a),
        target: mid(b),
        edge_type: EdgeType::Entity,
        weight: 1.0,
        auto_created: true,
        created_at: t(T0),
    };
    s.insert_edge(edge(1, 2));
    s.insert_edge(edge(1, 4));
    s.fade_to_ghost(mid(3), t(T0));
    let now = t(T0 + 30 * crate::time::MILLIS_PER_DAY);
    let stats = s
        .namespace_stats(&json(serde_json::json!({})), now)
        .unwrap();
    assert_eq!(stats.name, "default");
    assert_eq!(stats.memory_count, 3);
    assert_eq!(
        (
            stats.phase_counts.full,
            stats.phase_counts.summary,
            stats.phase_counts.ghost
        ),
        (2, 0, 1)
    );
    assert_eq!(stats.edge_count, 1, "only edges inside the namespace");
    assert_eq!((stats.embedding.ready, stats.embedding.none), (2, 1));
    assert_eq!(
        stats.text_bytes,
        ("alpha".len() + "beta gamma".len() + 12) as u64
    );
    let m = s.memory(mid(1)).unwrap();
    assert!((stats.average_strength - s.raw_strength(m, now)).abs() < 1e-6);
    assert!(stats.average_strength < 1.0);
    assert_eq!(stats.oldest_created_at, Some(t(T0 + 1_000)));
    assert_eq!(stats.newest_created_at, Some(t(T0 + 3_000)));
    let empty = StoreBuilder::new(FakeProvider::new().identity().clone(), t(T0)).build();
    let none = empty
        .namespace_stats(&json(serde_json::json!({})), now)
        .unwrap();
    assert_eq!((none.average_strength, none.oldest_created_at), (0.0, None));
}

#[test]
fn context_without_a_topic_lists_the_newest_with_three_full_texts() {
    let mut s = store(&[
        (1, NS, "a", &[]),
        (2, NS, "b", &[]),
        (3, NS, "c", &[]),
        (4, NS, "d", &[]),
        (5, NS, "e", &[]),
        (6, OTHER, "f", &[]),
    ]);
    s.fade_to_ghost(mid(5), t(T0));
    let ctx = s
        .context(&json(serde_json::json!({ "topic": "  " })), None, t(T0))
        .unwrap();
    let got: Vec<_> = ctx.memories.iter().map(|m| m.id).collect();
    assert_eq!(got, [mid(4), mid(3), mid(2), mid(1)]);
    assert_eq!(
        ctx.memories
            .iter()
            .map(|m| m.full_text.is_some())
            .collect::<Vec<_>>(),
        [true, true, true, false]
    );
    assert_eq!(ctx.default_namespace, "default");
    assert_eq!(ctx.namespaces.len(), 2);
    assert_eq!((ctx.embedding.pending, ctx.embedding.total), (0, 5));
    assert_eq!(ctx.degraded, None);
    for limit in [0, 26] {
        let err = s
            .context(&json(serde_json::json!({ "limit": limit })), None, t(T0))
            .unwrap_err();
        assert_eq!(err.field(), Some("limit"));
    }
}

#[test]
fn context_with_a_topic_runs_a_recall() {
    let s = store(&[
        (1, NS, "window seats", &[]),
        (2, NS, "revenue", &[]),
        (3, OTHER, "window", &[]),
    ]);
    let req: GetContext = json(serde_json::json!({ "topic": "window", "limit": 1 }));
    let ctx = s.context(&req, Some(&vector("window")), t(T0)).unwrap();
    assert_eq!(ctx.memories.len(), 1);
    assert_eq!(ctx.memories[0].id, mid(1));
    assert!(ctx.memories[0].full_text.is_some());
    let degraded = s.context(&req, None, t(T0)).unwrap();
    assert_eq!(degraded.degraded, Some(Degraded::ModelUnavailable));
    let long: GetContext = json(serde_json::json!({ "topic": "x".repeat(4_097) }));
    let err = s.context(&long, None, t(T0)).unwrap_err();
    assert_eq!(err.field(), Some("topic"));
    assert!(err.to_string().starts_with("topic is 4097 bytes"), "{err}");
    let other: GetContext = json(serde_json::json!({ "topic": "window", "namespace": "other" }));
    let ctx = s.context(&other, Some(&vector("window")), t(T0)).unwrap();
    assert_eq!(ctx.default_namespace, "other");
    assert_eq!(ctx.memories[0].id, mid(3));
}
