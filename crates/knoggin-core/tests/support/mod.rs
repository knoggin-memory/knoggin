// SPDX-License-Identifier: AGPL-3.0-only
//! Shared by the property tests: the word list, the random operation generator and the
//! harness that applies operations to a store.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use knoggin_core::embed::EmbeddingProvider;
use knoggin_core::model::{AccessKind, EmbeddingState};
use knoggin_core::ops::{
    AccessHit, ChangeCounters, DeleteNamespace, ForgetMemories, ForgetMemory, MoveMemories,
    ReinforceMemory, StoreMemories, StoreMemory, SupersedeMemory, UpdatePolicy, WriteContext,
};
use knoggin_core::store::IdempotencyEntry;
use knoggin_core::testing::FakeProvider;
use knoggin_core::transfer::{OnConflict, TransferRecord};
use knoggin_core::{EngineError, MemoryId, Store, StoreId, Timestamp};
use proptest::prelude::*;
use serde_json::{Value, json};

/// Every word of every generated memory comes from this list.
const WORDS: [&str; 50] = [
    "maya", "lisbon", "window", "seat", "travel", "coffee", "morning", "river", "garden", "piano",
    "budget", "report", "meeting", "friday", "train", "ticket", "hotel", "beach", "summer",
    "winter", "dinner", "recipe", "pasta", "tomato", "basil", "garlic", "doctor", "tooth",
    "dentist", "project", "deadline", "launch", "rocket", "engine", "memory", "search", "graph",
    "vector", "index", "alpha", "beta", "gamma", "delta", "omega", "red", "green", "blue", "quiet",
    "loud", "lamp",
];

/// The pool of namespaces; `default` always exists.
const NAMESPACES: [&str; 3] = ["default", "work", "Life"];
/// The pool of memory IDs.
const POOL: u8 = 30;
const T0: i64 = 1_700_000_000_000;
const DAY: i64 = 86_400_000;
const MULTIPLIERS: [Option<f32>; 5] = [None, Some(0.0), Some(0.01), Some(1.0), Some(50.0)];
/// Names of imported namespace lines: the pool, then one that is not a valid name.
const IMPORTED_NAMES: [&str; 4] = ["default", "work", "Life", "not valid!"];

fn mid(slot: u8) -> MemoryId {
    let mut b = [0_u8; 16];
    b[0] = 0x01;
    b[6] = 0x70;
    b[15] = slot;
    MemoryId::from_bytes(b)
}

fn text(words: &[u8]) -> String {
    words
        .iter()
        .map(|w| WORDS[usize::from(*w) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

fn ns(i: u8) -> &'static str {
    NAMESPACES[usize::from(i) % NAMESPACES.len()]
}

fn ctx() -> WriteContext {
    WriteContext {
        saved_by: "local".into(),
        client: Some("proptest".into()),
    }
}

#[derive(Debug, Clone)]
pub struct Content {
    summary: Vec<u8>,
    full: Option<Vec<u8>>,
    tags: Vec<u8>,
    entities: Vec<u8>,
    /// Days before now; 2 days ahead is refused.
    created_days_ago: Option<i64>,
}

impl Content {
    fn body(&self) -> Value {
        let mut v = json!({
            "summary": text(&self.summary),
            "tags": self.tags.iter().map(|w| text(&[*w])).collect::<Vec<_>>(),
            "entities": self.entities.iter().map(|w| {
                let w = text(&[*w]);
                // Mixed forms exercise entity normalization.
                if w.len().is_multiple_of(2) { format!("Dr. {w}'s") } else { w }
            }).collect::<Vec<_>>(),
        });
        if let Some(full) = &self.full {
            v["fullText"] = json!(text(full));
        }
        v
    }
}

#[derive(Debug, Clone)]
pub enum Op {
    Save {
        slot: u8,
        ns: u8,
        content: Content,
        parent: Option<u8>,
        vector: bool,
        check: bool,
    },
    Batch(Vec<(u8, u8, Content)>),
    Supersede {
        old: u8,
        slot: u8,
        content: Content,
        vector: bool,
    },
    Reinforce {
        slot: u8,
        quality: u8,
    },
    Forget {
        slot: u8,
        check: Option<u8>,
    },
    ForgetMany {
        slots: Vec<u8>,
        dry_run: bool,
    },
    Move {
        slots: Vec<u8>,
        to: u8,
        from: Option<u8>,
    },
    CreateNamespace(u8),
    UpdatePolicy {
        ns: u8,
        multiplier: u8,
        dry_run: bool,
    },
    DeleteNamespace {
        ns: u8,
        force: bool,
        dry_run: bool,
    },
    Access {
        slots: Vec<u8>,
        associative: bool,
    },
    Sweep {
        days: i64,
    },
    Apply(u8),
    ApplyInvalid,
    Fail(u8),
    AdoptModel,
    OnOpen,
    ImportMemory {
        slot: u8,
        ns: u8,
        phase: u8,
        content: Content,
    },
    ImportNamespace {
        name: u8,
        /// An index into `MULTIPLIERS`; one past the end is a negative multiplier.
        multiplier: Option<u8>,
        /// Days before now; 2 days ahead is refused.
        created_days_ago: Option<i64>,
    },
    ImportEdge {
        source: u8,
        target: u8,
        edge_type: u8,
        auto: bool,
    },
    Idempotent {
        key: u8,
        hash: u8,
        slot: u8,
        content: Content,
    },
}

fn words(max: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0..WORDS.len() as u8, 1..=max)
}

fn content() -> impl Strategy<Value = Content> {
    (
        words(6),
        prop::option::of(words(8)),
        prop::collection::vec(0..WORDS.len() as u8, 0..=3),
        prop::collection::vec(0..WORDS.len() as u8, 0..=3),
        prop::option::weighted(0.3, -2_i64..=400),
    )
        .prop_map(
            |(summary, full, tags, entities, created_days_ago)| Content {
                summary,
                full,
                tags,
                entities,
                created_days_ago,
            },
        )
}

fn slot() -> impl Strategy<Value = u8> {
    0..POOL
}

fn slots() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(slot(), 1..=6)
}

pub fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (slot(), 0..3_u8, content(), prop::option::weighted(0.2, slot()), any::<bool>(), any::<bool>())
            .prop_map(|(slot, ns, content, parent, vector, check)| Op::Save { slot, ns, content, parent, vector, check }),
        2 => prop::collection::vec((slot(), 0..4_u8, content()), 1..=4).prop_map(Op::Batch),
        2 => (slot(), slot(), content(), any::<bool>())
            .prop_map(|(old, slot, content, vector)| Op::Supersede { old, slot, content, vector }),
        2 => (slot(), 0..=5_u8).prop_map(|(slot, quality)| Op::Reinforce { slot, quality }),
        2 => (slot(), prop::option::weighted(0.3, 0..3_u8)).prop_map(|(slot, check)| Op::Forget { slot, check }),
        1 => (slots(), any::<bool>()).prop_map(|(slots, dry_run)| Op::ForgetMany { slots, dry_run }),
        2 => (slots(), 0..4_u8, prop::option::weighted(0.3, 0..3_u8))
            .prop_map(|(slots, to, from)| Op::Move { slots, to, from }),
        2 => (0..3_u8).prop_map(Op::CreateNamespace),
        1 => (0..3_u8, 0..MULTIPLIERS.len() as u8, any::<bool>())
            .prop_map(|(ns, multiplier, dry_run)| Op::UpdatePolicy { ns, multiplier, dry_run }),
        1 => (0..3_u8, any::<bool>(), any::<bool>())
            .prop_map(|(ns, force, dry_run)| Op::DeleteNamespace { ns, force, dry_run }),
        2 => (slots(), any::<bool>()).prop_map(|(slots, associative)| Op::Access { slots, associative }),
        2 => (0_i64..=4_000).prop_map(|days| Op::Sweep { days }),
        4 => (1..=5_u8).prop_map(Op::Apply),
        1 => Just(Op::ApplyInvalid),
        1 => slot().prop_map(Op::Fail),
        1 => Just(Op::AdoptModel),
        1 => Just(Op::OnOpen),
        2 => (slot(), 0..4_u8, 0..3_u8, content())
            .prop_map(|(slot, ns, phase, content)| Op::ImportMemory { slot, ns, phase, content }),
        2 => (0..IMPORTED_NAMES.len() as u8, prop::option::of(0..=MULTIPLIERS.len() as u8), prop::option::weighted(0.5, -2_i64..=400))
            .prop_map(|(name, multiplier, created_days_ago)| Op::ImportNamespace { name, multiplier, created_days_ago }),
        2 => (slot(), slot(), 0..5_u8, any::<bool>())
            .prop_map(|(source, target, edge_type, auto)| Op::ImportEdge { source, target, edge_type, auto }),
        2 => (0..3_u8, 0..2_u8, slot(), content())
            .prop_map(|(key, hash, slot, content)| Op::Idempotent { key, hash, slot, content }),
    ]
}

/// The store under test with the clock and the current fake model.
pub struct Harness {
    pub store: Store,
    pub now: i64,
    pub provider: FakeProvider,
}

impl Harness {
    pub fn new() -> Self {
        let provider = FakeProvider::new();
        Self {
            store: Store::create(
                StoreId::default(),
                provider.identity().clone(),
                Timestamp::from_millis(T0),
            ),
            now: T0,
            provider,
        }
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.now)
    }

    fn save_request(&self, c: &Content, namespace: Option<&str>) -> Value {
        let mut v = c.body();
        if let Some(n) = namespace {
            v["namespace"] = json!(n);
        }
        if let Some(days) = c.created_days_ago {
            let at = Timestamp::from_millis(self.now - days * DAY);
            v["createdAt"] = json!(at.to_rfc3339().unwrap());
        }
        v
    }

    /// An existing memory chosen by `slot`, so that operations on existing memories
    /// mostly get past their checks; the plain slot when the store is empty.
    fn pick(&self, slot: u8) -> MemoryId {
        let count = self.store.memories().count();
        if count == 0 {
            return mid(slot);
        }
        self.store
            .memories()
            .nth(usize::from(slot) % count)
            .map_or(mid(slot), |m| m.id)
    }

    fn vector_for(&self, c: &Content, on: bool) -> Option<Vec<f32>> {
        on.then(|| self.provider.vector(&text(&c.summary)))
    }

    /// Applies one step. `Err` means the operation refused, which must change nothing.
    pub fn apply(&mut self, op: &Op) -> Result<(), EngineError> {
        let now = self.now();
        match op {
            Op::Save {
                slot,
                ns: n,
                content,
                parent,
                vector,
                check,
            } => {
                let mut v = self.save_request(content, Some(ns(*n)));
                v["checkDuplicates"] = json!(check);
                if let Some(p) = parent {
                    v["parentId"] = json!(mid(*p));
                }
                let req: StoreMemory = serde_json::from_value(v).unwrap();
                let vector = self.vector_for(content, *vector);
                self.store
                    .store_memory(req, mid(*slot), vector, &ctx(), now)
                    .map(drop)
            }
            Op::Batch(items) => {
                let memories: Vec<Value> = items
                    .iter()
                    .map(|(_, n, c)| {
                        self.save_request(
                            c,
                            Some(NAMESPACES.get(usize::from(*n)).copied().unwrap_or("gone")),
                        )
                    })
                    .collect();
                let req: StoreMemories =
                    serde_json::from_value(json!({ "memories": memories })).unwrap();
                let ids = items.iter().map(|(slot, _, _)| mid(*slot)).collect();
                self.store.store_memories(req, ids, &ctx(), now).map(drop)
            }
            Op::Supersede {
                old,
                slot,
                content,
                vector,
            } => {
                let mut req: SupersedeMemory =
                    serde_json::from_value(self.save_request(content, None)).unwrap();
                req.id = self.pick(*old);
                let vector = self.vector_for(content, *vector);
                self.store
                    .supersede_memory(req, mid(*slot), vector, &ctx(), now)
                    .map(drop)
            }
            Op::Reinforce { slot, quality } => {
                let mut req: ReinforceMemory =
                    serde_json::from_value(json!({ "quality": quality })).unwrap();
                req.id = self.pick(*slot);
                self.store.reinforce_memory(req, &ctx(), now).map(drop)
            }
            Op::Forget { slot, check } => {
                let req = ForgetMemory {
                    id: mid(*slot),
                    namespace: check.map(|n| ns(n).to_owned()),
                };
                self.store.forget_memory(req, &ctx(), now).map(drop)
            }
            Op::ForgetMany { slots, dry_run } => {
                let req = ForgetMemories {
                    ids: slots.iter().map(|x| mid(*x)).collect(),
                    dry_run: *dry_run,
                };
                self.store.forget_memories(req, &ctx(), now).map(drop)
            }
            Op::Move { slots, to, from } => {
                let mut ids: Vec<MemoryId> = slots.iter().map(|x| self.pick(*x)).collect();
                ids.sort();
                ids.dedup();
                let req = MoveMemories {
                    ids,
                    destination_namespace: NAMESPACES
                        .get(usize::from(*to))
                        .copied()
                        .unwrap_or("gone")
                        .to_owned(),
                    source_namespace: from.map(|n| ns(n).to_owned()),
                };
                self.store.move_memories(req, &ctx(), now).map(drop)
            }
            Op::CreateNamespace(n) => {
                let req = serde_json::from_value(json!({ "name": ns(*n) })).unwrap();
                self.store.create_namespace(req, &ctx(), now).map(drop)
            }
            Op::UpdatePolicy {
                ns: n,
                multiplier,
                dry_run,
            } => {
                let policy = knoggin_core::model::NamespacePolicy {
                    decay_rate_multiplier: MULTIPLIERS[usize::from(*multiplier)],
                    ..Default::default()
                };
                let req = UpdatePolicy {
                    name: ns(*n).to_owned(),
                    policy,
                    dry_run: *dry_run,
                };
                self.store
                    .update_namespace_policy(req, &ctx(), now)
                    .map(drop)
            }
            Op::DeleteNamespace {
                ns: n,
                force,
                dry_run,
            } => {
                let req = DeleteNamespace {
                    name: ns(*n).to_owned(),
                    force: *force,
                    dry_run: *dry_run,
                };
                self.store.delete_namespace(req, &ctx(), now).map(drop)
            }
            Op::Access { slots, associative } => {
                let kind = if *associative {
                    AccessKind::Associative
                } else {
                    AccessKind::Direct
                };
                let hits: Vec<AccessHit> = slots
                    .iter()
                    .map(|x| AccessHit { id: mid(*x), kind })
                    .collect();
                self.store.record_access(&hits, now);
                Ok(())
            }
            Op::Sweep { days } => {
                self.now += days * DAY;
                let now = self.now();
                self.store.sweep(now);
                Ok(())
            }
            Op::Apply(n) => {
                let model = self.provider.identity().clone();
                for job in self.store.pending_embeddings(usize::from(*n)) {
                    let vector = self.provider.embed(&job.text).unwrap();
                    self.store.apply_embedding(job.id, &model, vector, now);
                }
                Ok(())
            }
            Op::ApplyInvalid => {
                let model = self.provider.identity().clone();
                if let Some(job) = self.store.pending_embeddings(1).pop() {
                    self.store
                        .apply_embedding(job.id, &model, vec![0.5; 3], now);
                }
                Ok(())
            }
            Op::Fail(slot) => {
                self.store.fail_embedding(mid(*slot));
                Ok(())
            }
            Op::AdoptModel => {
                let dim = if self.provider.identity().dimensions == 64 {
                    32
                } else {
                    64
                };
                self.provider = FakeProvider::with_dim(dim);
                self.store.adopt_model(self.provider.identity().clone());
                Ok(())
            }
            Op::OnOpen => {
                self.store.on_open(now);
                Ok(())
            }
            Op::ImportMemory {
                slot,
                ns: n,
                phase,
                content,
            } => {
                let mut v = self.save_request(content, None);
                v["type"] = json!("memory");
                v["id"] = json!(mid(*slot));
                v["namespace"] = json!(NAMESPACES.get(usize::from(*n)).copied().unwrap_or("gone"));
                if v.get("createdAt").is_none() {
                    v["createdAt"] = json!(now.to_rfc3339().unwrap());
                }
                v["phase"] = json!(["full", "summary", "ghost"][usize::from(*phase)]);
                if *phase >= 1 {
                    v.as_object_mut().unwrap().remove("fullText");
                }
                if *phase == 2 {
                    v["summary"] = Value::Null;
                    v["tags"] = json!([]);
                    v["entities"] = json!([]);
                }
                let record: TransferRecord = serde_json::from_value(v).unwrap();
                self.store
                    .import_record(record, OnConflict::Error, now)
                    .map(drop)
            }
            Op::ImportNamespace {
                name,
                multiplier,
                created_days_ago,
            } => {
                let mut v =
                    json!({ "type": "namespace", "name": IMPORTED_NAMES[usize::from(*name)] });
                if let Some(m) = multiplier {
                    let policy = knoggin_core::model::NamespacePolicy {
                        decay_rate_multiplier: MULTIPLIERS
                            .get(usize::from(*m))
                            .copied()
                            .unwrap_or(Some(-1.0)),
                        ..Default::default()
                    };
                    v["policy"] = serde_json::to_value(policy).unwrap();
                }
                if let Some(days) = created_days_ago {
                    let at = Timestamp::from_millis(self.now - days * DAY);
                    v["createdAt"] = json!(at.to_rfc3339().unwrap());
                }
                let record: TransferRecord = serde_json::from_value(v).unwrap();
                self.store
                    .import_record(record, OnConflict::Error, now)
                    .map(drop)
            }
            Op::ImportEdge {
                source,
                target,
                edge_type,
                auto,
            } => {
                let types = [
                    "parent_child",
                    "associative",
                    "entity",
                    "temporal",
                    "supersedes",
                ];
                let record: TransferRecord = serde_json::from_value(json!({
                    "type": "edge", "source": mid(*source), "target": mid(*target),
                    "edgeType": types[usize::from(*edge_type)], "autoCreated": auto, "weight": 0.5,
                }))
                .unwrap();
                self.store
                    .import_record(record, OnConflict::Skip, now)
                    .map(drop)
            }
            Op::Idempotent {
                key,
                hash,
                slot,
                content,
            } => {
                let mut v = self.save_request(content, None);
                v["checkDuplicates"] = json!(true);
                let req: StoreMemory = serde_json::from_value(v).unwrap();
                let vector = self.vector_for(content, true);
                let id = mid(*slot);
                self.store
                    .run_idempotent(
                        &format!("k{key}"),
                        [*hash; 32],
                        now,
                        move |s: &mut Store| {
                            let r = s.store_memory(req, id, vector, &ctx(), now)?;
                            let ids = r
                                .near_duplicates
                                .iter()
                                .flat_map(|d| d.matches.iter().map(|m| m.id))
                                .collect();
                            Ok((r, ids))
                        },
                    )
                    .map(drop)
            }
        }
    }

    /// Everything a failed step must leave alone.
    pub fn state(&self) -> State {
        let mut lines = Vec::new();
        let mut after = None;
        loop {
            let page = self
                .store
                .export_page(after, 1_000, Timestamp::from_millis(T0));
            lines.extend(
                page.records
                    .iter()
                    .map(|r| serde_json::to_string(r).unwrap()),
            );
            match page.next {
                Some(c) => after = Some(c),
                None => break,
            }
        }
        State {
            lines,
            changes: self.store.changes(),
            embeddings: self
                .store
                .memories()
                .map(|m| (m.id, m.embedding, m.embed_failures))
                .collect(),
            vectors: self
                .store
                .vectors()
                .iter()
                .map(|(id, v)| (id, v.iter().map(|x| x.to_bits()).collect()))
                .collect(),
            idempotency: self
                .store
                .idempotency_for_test()
                .clone()
                .into_iter()
                .collect(),
            estimated_bytes: self.store.estimated_bytes(),
        }
    }
}

/// The observable state of a store: every exported line, both counters, every
/// embedding state with its failure count, the vectors, the idempotency table and
/// the size estimate.
#[derive(Debug, PartialEq)]
pub struct State {
    lines: Vec<String>,
    changes: ChangeCounters,
    embeddings: Vec<(MemoryId, EmbeddingState, u8)>,
    pub vectors: Vec<(MemoryId, Vec<u32>)>,
    idempotency: Vec<(String, IdempotencyEntry)>,
    estimated_bytes: u64,
}
