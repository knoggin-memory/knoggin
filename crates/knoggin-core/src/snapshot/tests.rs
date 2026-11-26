// SPDX-License-Identifier: AGPL-3.0-only
use serde_json::json;

use super::*;
use crate::embed::EmbeddingProvider;
use crate::ops::{AccessHit, ReinforceMemory};
use crate::store::write::tests::{DAY, T0, ctx, mid, namespace, req, save, store, t, vector};
use crate::testing::FakeProvider;

const QUERIES: [&str; 4] = ["maya", "window", "seats", "maya window"];

/// A store with a vector, an edge, labels, history, a failed embedding and an entry.
fn populated() -> Store {
    let mut s = store();
    namespace(&mut s, "work");
    s.store_memory(
        req(json!({
            "summary": "maya likes window seats", "fullText": "maya likes window seats on trains",
            "tags": ["travel"], "entities": ["Maya"], "topics": ["Seats"], "emotions": ["calm"],
        })),
        mid(1),
        Some(vector("maya likes window seats")),
        &ctx(),
        t(T0),
    )
    .unwrap();
    save(&mut s, 2, "maya likes aisle seats");
    for _ in 0..3 {
        s.fail_embedding(mid(2));
    }
    s.record_access(
        &[AccessHit {
            id: mid(1),
            kind: AccessKind::Direct,
        }],
        t(T0 + 1),
    );
    let mut r: ReinforceMemory = serde_json::from_value(json!({ "quality": 4 })).unwrap();
    r.id = mid(1);
    s.reinforce_memory(r, &ctx(), t(T0 + DAY)).unwrap();
    s.run_idempotent("k1", [7; 32], t(T0), |s: &mut Store| {
        Ok((save(s, 3, "maya again"), vec![mid(1)]))
    })
    .unwrap();
    s
}

/// The six sections of format-1 content.
fn split(content: &[u8]) -> [Vec<u8>; 6] {
    sections(content).unwrap().map(<[u8]>::to_vec)
}

/// Format-1 content from six sections.
fn join(parts: &[Vec<u8>; 6]) -> Vec<u8> {
    let mut out = (parts.len() as u32).to_le_bytes().to_vec();
    for (id, p) in SECTIONS.iter().zip(parts) {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(p.len() as u64).to_le_bytes());
    }
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

fn corrupt(content: &[u8]) -> &'static str {
    match Store::decode(1, content) {
        Err(SnapshotError::Corrupt(what)) => what,
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn empty_store_layout() {
    let s = store();
    let bytes = s.encode();
    assert_eq!(&bytes[..4], &6_u32.to_le_bytes());
    let parts = split(&bytes);
    // An empty store: one namespace, no memories, a vectors header, no edges or entries.
    assert_eq!(parts[2], [0]);
    let mut vectors = 64_u32.to_le_bytes().to_vec();
    vectors.extend_from_slice(&0_u32.to_le_bytes());
    assert_eq!(parts[3], vectors);
    assert_eq!(
        (parts[4].as_slice(), parts[5].as_slice()),
        (&[0][..], &[0][..])
    );
    let back = Store::decode(1, &bytes).unwrap();
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.namespaces().count(), 1);
}

#[test]
fn round_trip_is_byte_exact_and_rebuilds_every_index() {
    let s = populated();
    let bytes = s.encode();
    assert_eq!(bytes, s.encode(), "the same store gives the same bytes");
    let back = Store::decode(1, &bytes).unwrap();
    assert_eq!(back.encode(), bytes);
    assert_eq!(back.invariant_violations_for_test(), Vec::<String>::new());
    assert_eq!(
        back.index_snapshot_for_test(&QUERIES),
        s.index_snapshot_for_test(&QUERIES)
    );
    assert_eq!(back.created_at(), s.created_at());
    assert_eq!(back.changes(), s.changes());
    assert_eq!(back.estimated_bytes(), s.estimated_bytes());
    let ns = back.namespace_by_name("default").unwrap().id;
    let hits = back.fts().search(ns, "window", 10).unwrap();
    assert_eq!(hits.first().map(|h| h.0), Some(mid(1)));
    let m = back.memory(mid(2)).unwrap();
    assert_eq!((m.embedding, m.embed_failures), (EmbeddingState::Failed, 3));
    assert_eq!(back.vector(mid(1)), s.vector(mid(1)));
    // decode changes nothing: on_open is what queues the failed memory again.
    let mut opened = back;
    opened.on_open(t(T0 + DAY));
    assert_eq!(
        opened.memory(mid(2)).unwrap().embedding,
        EmbeddingState::Pending
    );
}

#[test]
fn streamed_sequences_are_plain_postcard() {
    let s = populated();
    let parts = split(&s.encode());
    let memories: Vec<MemoryV1> = s.memories.values().map(memory_v1).collect();
    assert_eq!(parts[2], postcard::to_allocvec(&memories).unwrap());
    let namespaces: Vec<NamespaceV1> = s.namespaces.values().map(namespace_v1).collect();
    assert_eq!(parts[1], postcard::to_allocvec(&namespaces).unwrap());
}

#[test]
fn format_versions() {
    let bytes = store().encode();
    let err = Store::decode(2, &bytes).unwrap_err();
    assert_eq!(err, SnapshotError::FormatTooNew(2));
    assert_eq!(err.lock_reason(), "format_too_new");
    assert_eq!(corrupt_format(0, &bytes), "unknown content format");
    assert_eq!(
        SnapshotError::Corrupt("x").lock_reason(),
        "snapshot_unreadable"
    );
    assert_eq!(
        (WRITE_CONTENT_FORMAT, MIN_CONTENT_FORMAT, MAX_CONTENT_FORMAT),
        (1, 1, 1)
    );
}

fn corrupt_format(format: u32, content: &[u8]) -> &'static str {
    match Store::decode(format, content) {
        Err(SnapshotError::Corrupt(what)) => what,
        other => panic!("{other:?}"),
    }
}

#[test]
fn table_errors() {
    let good = populated().encode();
    let mut extra = good.clone();
    extra.push(0);
    assert_eq!(corrupt(&extra), "section table");
    assert_eq!(corrupt(&good[..good.len() - 1]), "section table");
    assert_eq!(corrupt(&good[..10]), "section table");
    let mut count = good.clone();
    count[0] = 5;
    assert_eq!(corrupt(&count), "section table");
    // Two section IDs swapped.
    let mut order = good.clone();
    order[4] = 2;
    order[16] = 1;
    assert_eq!(corrupt(&order), "section table");
    // A section with a byte after its value, its length counted.
    let mut parts = split(&good);
    parts[4].push(0);
    assert_eq!(corrupt(&join(&parts)), "edges section");
    assert_eq!(
        Store::decode(1, &join(&split(&good))).unwrap().encode(),
        good
    );
}

/// Decodes one postcard section of `good`, changes it, and puts it back.
fn edit<T: Serialize + DeserializeOwned>(
    good: &[u8],
    at: usize,
    f: impl FnOnce(&mut T),
) -> Vec<u8> {
    let mut parts = split(good);
    let mut value: T = postcard::from_bytes(&parts[at]).unwrap();
    f(&mut value);
    parts[at] = postcard::to_allocvec(&value).unwrap();
    join(&parts)
}

#[test]
fn record_checks() {
    let good = populated().encode();
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[0].phase = 4),
            "unknown phase",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[0].embedding = 9),
            "unknown embedding state",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| {
                m[0].access_history[0].kind = 3
            }),
            "unknown access kind",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[0].namespace = 9),
            "memory in an unknown namespace",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| {
                m[0].tags = vec!["Travel".into()]
            }),
            "tag not in lower case",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m.swap(0, 1)),
            "memories not sorted by id",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[0].embedding = 1),
            "vector count differs from ready memories",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| {
                m[1].phase = 3;
                m[1].embedding = 0;
            }),
            "ghost with content",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[1].embedding = 0),
            "embedding state does not fit the phase",
        ),
        (
            edit(&good, 2, |m: &mut Vec<MemoryV1>| m[0].stability = f32::NAN),
            "lifecycle number not finite",
        ),
        (
            edit(&good, 1, |n: &mut Vec<NamespaceV1>| {
                n[1].name = "DEFAULT".into()
            }),
            "namespace names repeat ignoring case",
        ),
        (
            edit(&good, 1, |n: &mut Vec<NamespaceV1>| {
                n[0].full_to_summary = 2.0
            }),
            "invalid namespace policy",
        ),
        (
            edit(&good, 1, |n: &mut Vec<NamespaceV1>| {
                n[0].name = "home".into()
            }),
            "no default namespace",
        ),
        (
            edit(&good, 0, |m: &mut MetaV1| m.next_namespace_id = 2),
            "namespace id out of range",
        ),
        (
            edit(&good, 4, |e: &mut Vec<EdgeV1>| e[0].edge_type = 3),
            "unknown edge type",
        ),
        (
            edit(&good, 4, |e: &mut Vec<EdgeV1>| e[0].target = [9; 16]),
            "edge to an unknown memory",
        ),
        (
            edit(&good, 4, |e: &mut Vec<EdgeV1>| e[0].target = e[0].source),
            "edge is a loop",
        ),
        (
            edit(&good, 5, |e: &mut Vec<IdempotencyV1>| {
                e[0].key = String::new()
            }),
            "invalid idempotency key",
        ),
        (
            edit(&good, 0, |m: &mut MetaV1| m.dimension = 32),
            "vector dimension differs from meta",
        ),
    ];
    for (bytes, what) in cases {
        assert_eq!(corrupt(&bytes), what);
    }
}

#[test]
fn vector_checks() {
    let good = populated().encode();
    let mut parts = split(&good);
    // A vector value that is not finite.
    let at = parts[3].len() - 4;
    parts[3][at..].copy_from_slice(&f32::NAN.to_le_bytes());
    assert_eq!(corrupt(&join(&parts)), "invalid vector");
    // A vector of another memory than the ready one.
    let mut parts = split(&good);
    parts[3][8..24].copy_from_slice(mid(2).as_bytes());
    assert_eq!(
        corrupt(&join(&parts)),
        "vector of a memory that is not ready"
    );
    // A count that does not match the section length.
    let mut parts = split(&good);
    parts[3][4] = 2;
    assert_eq!(corrupt(&join(&parts)), "vectors section");
}

#[test]
fn a_changed_model_round_trips() {
    let mut s = populated();
    s.adopt_model(FakeProvider::with_dim(32).identity().clone());
    let bytes = s.encode();
    let back = Store::decode(1, &bytes).unwrap();
    assert_eq!(back.model().dimensions, 32);
    assert_eq!(back.encode(), bytes);
}

#[test]
fn names() {
    let n = SnapshotName::new(3, 42).unwrap();
    assert_eq!(n.to_string(), "0000000003-0000000042");
    assert_eq!(n.file_name(), "0000000003-0000000042.kgs");
    assert_eq!(n.temp_file_name(), "0000000003-0000000042.kgs.tmp");
    assert_eq!("0000000003-0000000042".parse(), Ok(n));
    assert_eq!(
        SnapshotName::from_file_name("0000000003-0000000042.kgs"),
        Ok(n)
    );
    for bad in [
        "3-42",
        "0000000003-42",
        "0000000003_0000000042",
        "000000000a-0000000042",
        "+000000003-0000000042",
        "0000000003-0000000042-1",
        "",
    ] {
        assert_eq!(
            bad.parse::<SnapshotName>(),
            Err(NameError::Malformed),
            "{bad}"
        );
    }
    for bad in [
        "0000000003-0000000042.kgs.tmp",
        "0000000003-0000000042",
        "x.kgs",
    ] {
        assert_eq!(SnapshotName::from_file_name(bad), Err(NameError::Malformed));
    }
    let max = SnapshotName::new(9_999_999_999, 9_999_999_999).unwrap();
    assert_eq!(max.to_string().parse(), Ok(max));
    assert_eq!(
        SnapshotName::new(10_000_000_000, 1),
        Err(NameError::TooLarge)
    );
    assert_eq!(
        SnapshotName::new(1, 10_000_000_000),
        Err(NameError::TooLarge)
    );
    // Names sort as text exactly as they sort as numbers.
    let mut names = [(2, 1), (1, 10), (1, 9), (10, 0)]
        .map(|(g, s)| SnapshotName::new(g, s).unwrap())
        .to_vec();
    let mut texts: Vec<String> = names.iter().map(ToString::to_string).collect();
    names.sort();
    texts.sort();
    assert_eq!(
        names.iter().map(ToString::to_string).collect::<Vec<_>>(),
        texts
    );
}

mod thinning {
    use super::*;

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    /// 2023-11-14T12:05:00Z.
    const NOW: i64 = 1_699_963_500_000;

    fn obj(seq: u64, at: i64) -> SnapshotObject {
        SnapshotObject {
            name: SnapshotName::new(1, seq).unwrap(),
            last_modified: Timestamp::from_millis(at),
        }
    }

    fn name(seq: u64) -> SnapshotName {
        SnapshotName::new(1, seq).unwrap()
    }

    fn labels(
        objects: &[SnapshotObject],
        restore: Option<u64>,
        newest: Option<u64>,
        days: u32,
    ) -> Vec<Option<&'static str>> {
        thin(
            objects,
            restore.map(name),
            newest.map(name),
            days,
            Timestamp::from_millis(NOW),
        )
        .into_iter()
        .map(|l| l.map(KeptBecause::as_str))
        .collect()
    }

    #[test]
    fn nothing_and_one() {
        assert!(labels(&[], None, None, 7).is_empty());
        // The newest is kept however old it is.
        assert_eq!(labels(&[obj(1, 0)], None, None, 7), [Some("newest")]);
    }

    #[test]
    fn ten_minute_windows_of_the_last_hour() {
        // Now is 12:05. Window 12:00 holds 12:01 and 12:04; window 11:50 holds 11:52 and
        // 11:58; 11:04 is just over an hour ago, and its hour holds the higher 11:58.
        let objects = [
            obj(1, NOW - 61 * MIN),
            obj(2, NOW - 13 * MIN),
            obj(3, NOW - 7 * MIN),
            obj(4, NOW - 4 * MIN),
            obj(5, NOW - MIN),
        ];
        assert_eq!(
            labels(&objects, None, None, 7),
            [None, None, Some("last_hour"), None, Some("newest")]
        );
    }

    #[test]
    fn hourly_and_daily() {
        let objects = [
            obj(1, NOW - 3 * DAY),             // day -3: daily
            obj(2, NOW - 3 * DAY + HOUR),      // same day, higher name: daily wins over 1
            obj(3, NOW - 8 * DAY),             // outside 7 days
            obj(4, NOW - 30 * HOUR),           // yesterday, outside 24 h: daily
            obj(5, NOW - 5 * HOUR - 10 * MIN), // hour 06:00: lower name in its hour
            obj(6, NOW - 5 * HOUR - 20 * MIN), // hour 06:00 too, higher name
            obj(7, NOW - 2 * HOUR),            // hour 10:00
            obj(8, NOW),                       // newest
        ];
        assert_eq!(
            labels(&objects, None, None, 7),
            [
                None,
                Some("daily"),
                None,
                Some("daily"),
                None,
                Some("hourly"),
                Some("hourly"),
                Some("newest"),
            ]
        );
        // Today and the `days - 1` days before: with 1 day only today counts.
        assert_eq!(
            labels(&objects, None, None, 1),
            [
                None,
                None,
                None,
                None,
                None,
                Some("hourly"),
                Some("hourly"),
                Some("newest")
            ]
        );
        // No daily rule at all with 0 days.
        assert_eq!(labels(&objects[..2], None, None, 0), [None, Some("newest")]);
    }

    #[test]
    fn highest_name_wins_whatever_the_times() {
        // Out-of-order times: 3 has the oldest time but the highest name.
        let objects = [
            obj(1, NOW - 30 * MIN),
            obj(2, NOW - 2 * MIN),
            obj(3, NOW - 3 * HOUR),
        ];
        assert_eq!(
            labels(&objects, None, None, 7),
            [Some("last_hour"), Some("last_hour"), Some("newest")]
        );
        // 2 has a newer time than 3 in the same window; the higher name wins it.
        let objects = [obj(2, NOW - 2 * MIN), obj(3, NOW - 4 * MIN)];
        assert_eq!(labels(&objects, None, None, 7), [None, Some("newest")]);
    }

    #[test]
    fn recorded_newest_and_restore() {
        // Junk above the recorded newest: both keep the label `newest`.
        let objects = [
            obj(1, NOW - 9 * DAY),
            obj(2, NOW - 9 * DAY),
            obj(5, NOW - 9 * DAY),
        ];
        assert_eq!(
            labels(&objects, Some(1), Some(2), 7),
            [Some("restore"), Some("newest"), Some("newest")]
        );
        // `newest` comes before `restore`.
        assert_eq!(
            labels(&objects, Some(2), Some(2), 7),
            [None, Some("newest"), Some("newest")]
        );
    }

    #[test]
    fn first_reason_is_given_and_future_times_count_as_now() {
        // A time a day ahead is placed at now: in the 12:00 window, with 1 in it.
        let objects = [obj(1, NOW - 2 * MIN), obj(2, NOW + DAY)];
        assert_eq!(labels(&objects, None, None, 7), [None, Some("newest")]);
        // Kept by the 10-minute, hourly and daily rules at once: labelled `last_hour`.
        let objects = [obj(1, NOW + DAY), obj(2, NOW - 30 * MIN)];
        assert_eq!(
            labels(&objects, None, None, 7),
            [Some("last_hour"), Some("newest")]
        );
    }
}
