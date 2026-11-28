// SPDX-License-Identifier: AGPL-3.0-only
//! The memory writes: save, batch save, supersede, reinforce, forget, move, and
//! access recording. Each validates fully before it changes anything, so a failed
//! write leaves the store as it was.

use std::collections::BTreeSet;

use crate::decay::fsrs::review_stability;
use crate::error::EngineError;
use crate::id::MemoryId;
use crate::model::limits::{ACCESS_HISTORY_MAX, ContentRef, MAX_BATCH, validate_content};
use crate::model::{
    AccessEvent, AccessKind, Edge, EdgeType, EmbeddingState, Labels, Memory, Phase,
};
use crate::ops::{
    AccessHit, BatchItemResult, BatchResult, BatchStatus, ForgetItem, ForgetManyResult,
    ForgetMemories, ForgetMemory, ItemError, MoveMemories, Moved, ReinforceMemory, Reinforced,
    StoreMemories, StoreMemory, StoredMemory, SupersedeMemory, WriteContext,
};
use crate::search::vector::is_valid_vector;
use crate::time::{Timestamp, elapsed_days};

use super::Store;

/// What one save needs beyond its content.
struct SaveArgs<'a> {
    content: ContentRef<'a>,
    namespace: Option<&'a str>,
    parent: Option<MemoryId>,
    supersedes: Option<MemoryId>,
    check_duplicates: bool,
}

/// Appends an access event, keeping the newest `ACCESS_HISTORY_MAX`.
pub(crate) fn push_history(history: &mut Vec<AccessEvent>, event: AccessEvent) {
    history.push(event);
    let excess = history.len().saturating_sub(ACCESS_HISTORY_MAX);
    history.drain(..excess);
}

/// `count` as a `u32` for an answer; batches hold at most 100 items.
pub(crate) fn count_u32(count: usize, field: &'static str) -> Result<u32, EngineError> {
    u32::try_from(count).map_err(|_| EngineError::invalid(field, format!("{field} is too long")))
}

/// 1 to `MAX_BATCH` items.
pub(crate) fn check_batch_len(field: &'static str, len: usize) -> Result<(), EngineError> {
    if (1..=MAX_BATCH).contains(&len) {
        Ok(())
    } else {
        Err(EngineError::invalid(
            field,
            format!("{field} must hold 1 to {MAX_BATCH} items, got {len}"),
        ))
    }
}

impl Store {
    /// Whether a namespace that is only a check matches the memory's namespace.
    fn namespace_matches(&self, check: Option<&str>, memory: &Memory) -> bool {
        check.is_none_or(|name| {
            self.namespace_by_name(name)
                .is_some_and(|n| n.id == memory.namespace)
        })
    }

    pub(crate) fn note_client_write(&mut self, ctx: &WriteContext, now: Timestamp) {
        self.changes.content = self.changes.content.saturating_add(1);
        self.last_write_at = Some(now);
        self.last_write_client.clone_from(&ctx.client);
    }

    /// Saves one memory. `vector` is the document
    /// embedding when the host already has it. One that is not valid for the store's
    /// model counts as a failed embedding: the memory is saved `Pending` with one
    /// failure, as `apply_embedding` treats the same vector.
    pub fn store_memory(
        &mut self,
        req: StoreMemory,
        id: MemoryId,
        vector: Option<Vec<f32>>,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<StoredMemory, EngineError> {
        let args = SaveArgs {
            content: req.content(),
            namespace: req.namespace.as_deref(),
            parent: req.parent_id,
            supersedes: None,
            check_duplicates: req.check_duplicates,
        };
        self.save(args, id, vector, ctx, now)
    }

    /// Saves 1 to 100 memories in order, each on its own; a failed item does not stop
    /// the others. `ids` holds one new ID per item. Batch items never run the
    /// duplicate check.
    pub fn store_memories(
        &mut self,
        req: StoreMemories,
        ids: Vec<MemoryId>,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<BatchResult<StoredMemory>, EngineError> {
        check_batch_len("memories", req.memories.len())?;
        if ids.len() != req.memories.len() {
            return Err(EngineError::invalid(
                "memories",
                "one new ID is needed per item",
            ));
        }
        let mut results = Vec::with_capacity(ids.len());
        let (mut stored, mut errors) = (0_u32, 0_u32);
        for (index, (item, id)) in req.memories.into_iter().zip(ids).enumerate() {
            let index = count_u32(index, "memories")?;
            let outcome = self.store_memory(item.into_store_memory(), id, None, ctx, now);
            results.push(match outcome {
                Ok(memory) => {
                    stored = stored.saturating_add(1);
                    BatchItemResult {
                        index,
                        status: BatchStatus::Ok,
                        memory: Some(memory),
                        error: None,
                    }
                }
                Err(e) => {
                    errors = errors.saturating_add(1);
                    BatchItemResult {
                        index,
                        status: BatchStatus::Error,
                        memory: None,
                        error: Some(ItemError::from(&e)),
                    }
                }
            });
        }
        Ok(BatchResult {
            results,
            stored,
            errors,
        })
    }

    /// Saves a new memory in the namespace of `req.id` and adds a `Supersedes` edge
    /// from the new memory to the old one, which is not changed. A ghost cannot be
    /// superseded (`not_found`).
    pub fn supersede_memory(
        &mut self,
        req: SupersedeMemory,
        id: MemoryId,
        vector: Option<Vec<f32>>,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<StoredMemory, EngineError> {
        let args = SaveArgs {
            content: req.content(),
            namespace: None,
            parent: None,
            supersedes: Some(req.id),
            check_duplicates: req.check_duplicates,
        };
        self.save(args, id, vector, ctx, now)
    }

    fn save(
        &mut self,
        args: SaveArgs<'_>,
        id: MemoryId,
        vector: Option<Vec<f32>>,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<StoredMemory, EngineError> {
        // The checks that need no store.
        let tags = validate_content(args.content, now)?;
        // The namespace (a supersede takes the old memory's).
        let namespace = match args.supersedes {
            Some(old) => self
                .memories
                .get(&old)
                .filter(|m| m.phase != Phase::Ghost)
                .map(|m| m.namespace)
                .ok_or_else(|| EngineError::NotFound(format!("memory {old} not found")))?,
            None => self.resolve_namespace(args.namespace)?.id,
        };
        // The parent.
        if let Some(parent) = args.parent
            && !self
                .memories
                .get(&parent)
                .is_some_and(|p| p.namespace == namespace && p.phase != Phase::Ghost)
        {
            return Err(EngineError::NotFound(format!(
                "parent memory {parent} not found in the namespace"
            )));
        }
        if self.memories.contains_key(&id) {
            return Err(EngineError::Conflict(format!("memory {id} already exists")));
        }
        let policy = self
            .namespaces
            .get(&namespace)
            .map(|n| n.policy)
            .ok_or_else(|| EngineError::NotFound("namespace not found".into()))?;
        let (vector, invalid_vector) = match vector {
            Some(v) if is_valid_vector(&v, self.vectors.dimensions()) => (Some(v), false),
            Some(_) => (None, true),
            None => (None, false),
        };

        let near_duplicates = match (&vector, args.check_duplicates) {
            (Some(v), true) => self.duplicate_report(namespace, v, args.supersedes),
            _ => None,
        };
        let created_at = args.content.created_at.unwrap_or(now);
        let content = args.content;
        let memory = Memory {
            id,
            namespace,
            created_at,
            summary: content.summary.to_owned(),
            full_text: content.full_text.map(str::to_owned),
            tags,
            labels: Labels {
                entities: content.entities.to_vec(),
                topics: content.topics.to_vec(),
                emotions: content.emotions.to_vec(),
            },
            phase: Phase::Full,
            stability: policy.initial_stability,
            difficulty: policy.default_difficulty,
            is_permastore: false,
            decay_anchor_at: created_at,
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: created_at,
            access_count: 0,
            access_history: Vec::new(),
            embedding: EmbeddingState::Pending,
            embed_failures: 0,
            saved_by: ctx.saved_by.clone(),
            client: ctx.client.clone(),
        };
        if !self.insert_memory(memory, vector.as_deref()) {
            return Err(EngineError::Conflict(format!("memory {id} already exists")));
        }

        let mut links = Vec::new();
        if let Some(parent) = args.parent {
            links.extend(self.add_manual_edge(parent, id, EdgeType::ParentChild, now));
        }
        links.extend(self.add_save_links(id, now));
        if invalid_vector {
            self.fail_embedding(id);
        }
        if vector.is_some() {
            links.extend(self.add_semantic_links(id, now));
        }
        if let Some(old) = args.supersedes {
            links.extend(self.add_manual_edge(id, old, EdgeType::Supersedes, now));
        }
        self.note_client_write(ctx, now);

        let memory = self
            .memories
            .get(&id)
            .ok_or_else(|| EngineError::NotFound(format!("memory {id} not found")))?;
        Ok(StoredMemory {
            id,
            namespace: self
                .namespaces
                .get(&namespace)
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            phase: memory.phase,
            strength: self.raw_strength(memory, now),
            stability: memory.stability,
            created_at,
            embedding: memory.embedding,
            near_duplicates,
            duplicate_check: None,
            supersedes: args.supersedes,
            links,
        })
    }

    fn add_manual_edge(
        &mut self,
        source: MemoryId,
        target: MemoryId,
        edge_type: EdgeType,
        now: Timestamp,
    ) -> Option<Edge> {
        let edge = Edge {
            source,
            target,
            edge_type,
            weight: 1.0,
            auto_created: false,
            created_at: now,
        };
        self.insert_edge(edge).then_some(edge)
    }

    /// Reinforces a memory with `quality` 1 to 4: the FSRS stability update, the decay
    /// anchor moves to now, the phase stays. A ghost is a `conflict`.
    pub fn reinforce_memory(
        &mut self,
        req: ReinforceMemory,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<Reinforced, EngineError> {
        if !(1..=4).contains(&req.quality) {
            return Err(EngineError::invalid(
                "quality",
                format!("quality must be 1 to 4, got {}", req.quality),
            ));
        }
        let memory = self
            .memories
            .get(&req.id)
            .ok_or_else(|| EngineError::NotFound(format!("memory {} not found", req.id)))?;
        if memory.phase == Phase::Ghost {
            return Err(EngineError::Conflict("content no longer available".into()));
        }
        let policy = self
            .namespaces
            .get(&memory.namespace)
            .map(|n| n.policy)
            .unwrap_or_default();
        let stability = review_stability(
            memory.stability,
            elapsed_days(now, memory.decay_anchor_at),
            req.quality,
            policy.multiplier(),
            memory.difficulty,
        );
        if !stability.is_finite() {
            return Err(EngineError::invalid(
                "quality",
                "the new stability is not finite",
            ));
        }
        let reached = stability >= policy.permastore_threshold;
        let (phase, is_permastore) = self
            .update_memory(req.id, |m| {
                m.stability = stability;
                m.decay_anchor_at = now;
                m.reinforced_at = Some(now);
                m.is_permastore = m.is_permastore || reached;
                push_history(
                    &mut m.access_history,
                    AccessEvent {
                        at: now,
                        kind: AccessKind::Reinforcement,
                    },
                );
                (m.phase, m.is_permastore)
            })
            .ok_or_else(|| EngineError::NotFound(format!("memory {} not found", req.id)))?;
        self.note_client_write(ctx, now);
        Ok(Reinforced {
            id: req.id,
            strength: 1.0,
            stability,
            phase,
            is_permastore,
        })
    }

    /// Removes a memory with its vector, edges and index entries. An unknown ID, or
    /// one outside the namespace given as a check, is `deleted: false`.
    pub fn forget_memory(
        &mut self,
        req: ForgetMemory,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<crate::ops::Forgotten, EngineError> {
        let found = self
            .memories
            .get(&req.id)
            .is_some_and(|m| self.namespace_matches(req.namespace.as_deref(), m));
        let deleted = found && self.remove_memory(req.id).is_some();
        if deleted {
            self.note_client_write(ctx, now);
        }
        Ok(crate::ops::Forgotten {
            id: req.id,
            deleted,
        })
    }

    /// Forgets 1 to 100 IDs; one entry per ID in order. A dry run reports which exist.
    pub fn forget_memories(
        &mut self,
        req: ForgetMemories,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<ForgetManyResult, EngineError> {
        check_batch_len("ids", req.ids.len())?;
        let mut results = Vec::with_capacity(req.ids.len());
        let mut seen = BTreeSet::new();
        let mut deleted_count = 0_u32;
        for (index, id) in req.ids.iter().copied().enumerate() {
            let deleted = if req.dry_run {
                seen.insert(id) && self.memories.contains_key(&id)
            } else {
                self.remove_memory(id).is_some()
            };
            if deleted {
                deleted_count = deleted_count.saturating_add(1);
            }
            results.push(ForgetItem {
                index: count_u32(index, "ids")?,
                id,
                deleted,
            });
        }
        if deleted_count > 0 && !req.dry_run {
            self.note_client_write(ctx, now);
        }
        Ok(ForgetManyResult {
            results,
            deleted: deleted_count,
        })
    }

    /// Moves 1 to 100 unique memories to another namespace, all or nothing. IDs,
    /// content, vectors, history, edges, stability and anchor are kept.
    pub fn move_memories(
        &mut self,
        req: MoveMemories,
        ctx: &WriteContext,
        now: Timestamp,
    ) -> Result<Moved, EngineError> {
        check_batch_len("ids", req.ids.len())?;
        let unique: BTreeSet<MemoryId> = req.ids.iter().copied().collect();
        if unique.len() != req.ids.len() {
            return Err(EngineError::invalid("ids", "ids must be unique"));
        }
        let destination = self.resolve_namespace(Some(&req.destination_namespace))?.id;
        let source = match req.source_namespace.as_deref() {
            Some(name) => Some(self.resolve_namespace(Some(name))?.id),
            None => None,
        };
        for id in &req.ids {
            let memory = self
                .memories
                .get(id)
                .ok_or_else(|| EngineError::NotFound(format!("memory {id} not found")))?;
            if memory.phase == Phase::Ghost {
                return Err(EngineError::Conflict(format!(
                    "memory {id} is a ghost and cannot be moved"
                )));
            }
            if source.is_some_and(|s| s != memory.namespace) {
                return Err(EngineError::Conflict(format!(
                    "memory {id} is not in the source namespace"
                )));
            }
        }
        let mut changed = false;
        for id in &req.ids {
            if self
                .memories
                .get(id)
                .is_some_and(|m| m.namespace != destination)
            {
                changed |= self.move_memory(*id, destination);
            }
        }
        if changed {
            self.note_client_write(ctx, now);
        }
        Ok(Moved {
            moved: count_u32(req.ids.len(), "ids")?,
            destination_namespace: self
                .namespaces
                .get(&destination)
                .map(|n| n.name.clone())
                .unwrap_or_default(),
        })
    }

    /// Records the hits of a recall, a get or a similar search: `retrieved_at`,
    /// the access count and the history. Ghosts and unknown IDs are skipped. Stability
    /// and the decay anchor never change here.
    pub fn record_access(&mut self, hits: &[AccessHit], now: Timestamp) {
        let mut changed = false;
        for hit in hits {
            let recorded = self.update_memory(hit.id, |m| {
                if m.phase == Phase::Ghost {
                    return false;
                }
                m.retrieved_at = Some(m.retrieved_at.map_or(now, |at| at.max(now)));
                m.access_count = m.access_count.saturating_add(1);
                push_history(
                    &mut m.access_history,
                    AccessEvent {
                        at: now,
                        kind: hit.kind,
                    },
                );
                true
            });
            changed |= recorded == Some(true);
        }
        if changed {
            self.changes.bookkeeping = self.changes.bookkeeping.saturating_add(1);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::id::{NamespaceId, StoreId};
    use crate::model::EdgeKey;
    use crate::ops::CreateNamespace;
    use crate::testing::FakeProvider;
    use serde_json::json;

    pub(crate) fn mid(n: u16) -> MemoryId {
        let mut b = [0; 16];
        b[0] = 1;
        b[14..].copy_from_slice(&n.to_be_bytes());
        MemoryId::from_bytes(b)
    }

    pub(crate) fn t(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    pub(crate) const T0: i64 = 1_700_000_000_000;
    pub(crate) const DAY: i64 = 86_400_000;

    pub(crate) fn ctx() -> WriteContext {
        WriteContext {
            saved_by: "local".into(),
            client: Some("test-client".into()),
        }
    }

    pub(crate) fn store() -> Store {
        Store::create(
            StoreId::default(),
            FakeProvider::new().identity().clone(),
            t(0),
        )
    }

    pub(crate) fn req(v: serde_json::Value) -> StoreMemory {
        serde_json::from_value(v).unwrap()
    }

    pub(crate) fn save(s: &mut Store, n: u16, summary: &str) -> StoredMemory {
        s.store_memory(
            req(json!({ "summary": summary })),
            mid(n),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap()
    }

    pub(crate) fn vector(text: &str) -> Vec<f32> {
        FakeProvider::new().vector(text)
    }

    pub(crate) fn assert_matches_rebuild(s: &mut Store) {
        let q = ["alpha", "beta", "maya", "alpha beta"];
        let live = s.index_snapshot_for_test(&q);
        s.rebuild_indexes_for_test();
        assert_eq!(live, s.index_snapshot_for_test(&q));
        assert_eq!(s.invariant_violations_for_test(), Vec::<String>::new());
    }

    pub(crate) fn namespace(s: &mut Store, name: &str) {
        let req: CreateNamespace = serde_json::from_value(json!({ "name": name })).unwrap();
        s.create_namespace(req, &ctx(), t(T0)).unwrap();
    }

    fn edge_types(s: &Store, id: MemoryId) -> Vec<EdgeType> {
        s.graph().edges_of(id).map(|e| e.edge_type).collect()
    }

    #[test]
    fn save_makes_a_full_pending_memory_with_namespace_defaults() {
        let mut s = store();
        let r = save(&mut s, 1, "alpha");
        assert_eq!(r.namespace, "default");
        assert_eq!(
            (r.phase, r.embedding, r.strength),
            (Phase::Full, EmbeddingState::Pending, 1.0)
        );
        assert_eq!(r.stability, 3.7145);
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(
            (m.created_at, m.decay_anchor_at, m.phase_changed_at),
            (t(T0), t(T0), t(T0))
        );
        assert_eq!(m.saved_by, "local");
        assert_eq!(m.client.as_deref(), Some("test-client"));
        assert_eq!(s.changes().content, 1);
        assert_eq!(
            s.summary().last_write_client.as_deref(),
            Some("test-client")
        );
        assert_eq!(s.summary().last_write_at, Some(t(T0)));
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn an_earlier_created_at_gives_a_lower_strength() {
        let mut s = store();
        let r = s
            .store_memory(
                req(json!({ "summary": "old", "createdAt": "2023-01-01T00:00:00Z" })),
                mid(1),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap();
        assert!(r.strength < 1.0);
        assert_eq!(s.memory(mid(1)).unwrap().decay_anchor_at, r.created_at);
    }

    #[test]
    fn validation_runs_in_order_and_changes_nothing() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        let before = (s.index_snapshot_for_test(&["alpha"]), s.changes());
        let cases = [
            (json!({ "summary": "" , "tags": ["Bad Tag"] }), "summary"),
            (
                json!({ "summary": "s", "fullText": "", "tags": ["Bad Tag"] }),
                "fullText",
            ),
            (
                json!({ "summary": "s", "tags": ["Bad Tag"], "entities": [""] }),
                "tags",
            ),
            (
                json!({ "summary": "s", "entities": [""], "createdAt": "2999-01-01T00:00:00Z" }),
                "entities",
            ),
            (
                json!({ "summary": "<tool_call>x", "createdAt": "2999-01-01T00:00:00Z" }),
                "createdAt",
            ),
        ];
        for (body, field) in cases {
            let err = s
                .store_memory(req(body), mid(2), None, &ctx(), t(T0))
                .unwrap_err();
            assert_eq!(err.field(), Some(field), "{err}");
        }
        let err = s
            .store_memory(
                req(json!({ "summary": "s", "namespace": "nope" })),
                mid(2),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::NotFound);
        assert_eq!(err.to_string(), "namespace 'nope' not found");
        let err = s
            .store_memory(
                req(json!({ "summary": "s", "parentId": mid(9) })),
                mid(2),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::NotFound);
        let err = s
            .store_memory(req(json!({ "summary": "s" })), mid(1), None, &ctx(), t(T0))
            .unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::Conflict, "an ID that exists");
        assert_eq!((s.index_snapshot_for_test(&["alpha"]), s.changes()), before);
    }

    #[test]
    fn the_namespace_is_found_ignoring_case_and_the_parent_must_share_it() {
        let mut s = store();
        namespace(&mut s, "Work");
        let r = s
            .store_memory(
                req(json!({ "summary": "a", "namespace": "work" })),
                mid(1),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap();
        assert_eq!(r.namespace, "Work");
        let err = s
            .store_memory(
                req(json!({ "summary": "b", "parentId": mid(1) })),
                mid(2),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap_err();
        assert_eq!(
            err.code(),
            crate::ErrorCode::NotFound,
            "parent in another namespace"
        );
        let r = s
            .store_memory(
                req(json!({ "summary": "b", "namespace": "Work", "parentId": mid(1) })),
                mid(2),
                None,
                &ctx(),
                t(T0 + 10 * DAY),
            )
            .unwrap();
        assert_eq!(r.links.len(), 1);
        let e = r.links[0];
        assert_eq!(
            (e.source, e.target, e.edge_type, e.weight, e.auto_created),
            (mid(1), mid(2), EdgeType::ParentChild, 1.0, false)
        );
    }

    #[test]
    fn save_links_entities_and_times_in_the_namespace_only() {
        let mut s = store();
        namespace(&mut s, "work");
        let body = |summary: &str, ns: &str, at: &str| {
            req(
                json!({ "summary": summary, "namespace": ns, "entities": ["Maya Smith"], "createdAt": at }),
            )
        };
        s.store_memory(
            body("a", "default", "2023-11-14T22:00:00Z"),
            mid(1),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            body("b", "work", "2023-11-14T22:00:00Z"),
            mid(2),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        let mut far = body("c", "default", "2023-01-01T00:00:00Z");
        far.entities = vec!["Lisbon".into()];
        s.store_memory(far, mid(3), None, &ctx(), t(T0)).unwrap();
        let r = s
            .store_memory(
                body("d", "default", "2023-11-14T22:30:00Z"),
                mid(4),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap();
        let mut got: Vec<(MemoryId, EdgeType, f32)> = r
            .links
            .iter()
            .map(|e| (e.target, e.edge_type, e.weight))
            .collect();
        got.sort_by_key(|x| (x.0, x.1));
        assert_eq!(
            got,
            [
                (mid(1), EdgeType::Entity, 1.0),
                (mid(1), EdgeType::Temporal, 0.5)
            ]
        );
        assert!(r.links.iter().all(|e| e.source == mid(4) && e.auto_created));
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn a_vector_with_the_save_links_semantically_and_reports_duplicates() {
        let mut s = store();
        for (n, text) in [
            (1, "maya likes window seats"),
            (2, "maya likes window seats too"),
            (3, "something else entirely"),
        ] {
            s.store_memory(
                req(json!({ "summary": text })),
                mid(n),
                Some(vector(text)),
                &ctx(),
                t(T0 + i64::from(n) * 10 * DAY),
            )
            .unwrap();
        }
        let text = "maya likes window seats";
        let r = s
            .store_memory(
                req(json!({ "summary": text, "checkDuplicates": true })),
                mid(4),
                Some(vector(text)),
                &ctx(),
                t(T0 + 40 * DAY),
            )
            .unwrap();
        assert_eq!(r.embedding, EmbeddingState::Ready);
        let report = r.near_duplicates.unwrap();
        assert_eq!(report.threshold, 0.85);
        assert_eq!(report.matches[0].id, mid(1));
        assert!((report.matches[0].score - 1.0).abs() < 1e-5);
        assert_eq!(report.matches[0].summary, text);
        assert!(report.matches.iter().all(|m| m.id != mid(3)));
        assert!(edge_types(&s, mid(4)).contains(&EdgeType::Associative));
        // Without the flag there is no report; an invalid vector counts as one failed
        // embedding and leaves it pending.
        let r = s
            .store_memory(
                req(json!({ "summary": text })),
                mid(5),
                Some(vector(text)),
                &ctx(),
                t(T0 + 50 * DAY),
            )
            .unwrap();
        assert!(r.near_duplicates.is_none());
        let r = s
            .store_memory(
                req(json!({ "summary": text, "checkDuplicates": true })),
                mid(6),
                Some(vec![1.0; 3]),
                &ctx(),
                t(T0 + 60 * DAY),
            )
            .unwrap();
        assert!(r.near_duplicates.is_none());
        assert_eq!(r.embedding, EmbeddingState::Pending);
        assert_eq!(s.memory(mid(6)).unwrap().embed_failures, 1);
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn duplicate_report_keeps_three_and_truncates_summaries() {
        let mut s = store();
        let long = format!("maya {}", "é".repeat(200));
        for n in 1..=4 {
            s.store_memory(
                req(json!({ "summary": long })),
                mid(n),
                Some(vector("maya")),
                &ctx(),
                t(T0 + i64::from(n) * 10 * DAY),
            )
            .unwrap();
        }
        let report = s
            .duplicate_report(NamespaceId::new(1), &vector("maya"), Some(mid(1)))
            .unwrap();
        let ids: Vec<_> = report.matches.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [mid(2), mid(3), mid(4)],
            "ties by ID, the excluded one left out"
        );
        let summary = &report.matches[0].summary;
        assert!(
            summary.len() <= 240 && summary.ends_with('\u{2026}'),
            "{}",
            summary.len()
        );
        assert!(
            s.duplicate_report(NamespaceId::new(1), &vector("unrelated words"), None)
                .is_none()
        );
    }

    #[test]
    fn supersede_saves_in_the_old_namespace_and_links_new_to_old() {
        let mut s = store();
        namespace(&mut s, "work");
        s.store_memory(
            req(json!({ "summary": "v1", "namespace": "work" })),
            mid(1),
            Some(vector("v1 seats")),
            &ctx(),
            t(T0),
        )
        .unwrap();
        let sup: SupersedeMemory = {
            let mut r: SupersedeMemory =
                serde_json::from_value(json!({ "summary": "v1 seats", "checkDuplicates": true }))
                    .unwrap();
            r.id = mid(1);
            r
        };
        let r = s
            .supersede_memory(sup.clone(), mid(2), Some(vector("v1 seats")), &ctx(), t(T0))
            .unwrap();
        assert_eq!(r.namespace, "work");
        assert_eq!(r.supersedes, Some(mid(1)));
        assert!(
            r.near_duplicates.is_none(),
            "the replaced memory is left out of the scan"
        );
        let key = EdgeKey {
            source: mid(2),
            edge_type: EdgeType::Supersedes,
            target: mid(1),
        };
        let e = s.graph().get(&key).unwrap();
        assert_eq!((e.weight, e.auto_created), (1.0, false));
        assert_eq!(s.changes().content, 3);
        let mut missing = sup.clone();
        missing.id = mid(9);
        let err = s
            .supersede_memory(missing, mid(3), None, &ctx(), t(T0))
            .unwrap_err();
        assert_eq!(err.code(), crate::ErrorCode::NotFound);
        assert!(s.fade_to_ghost(mid(1), t(T0)));
        let err = s
            .supersede_memory(sup, mid(3), None, &ctx(), t(T0))
            .unwrap_err();
        assert_eq!(
            err.code(),
            crate::ErrorCode::NotFound,
            "a ghost cannot be superseded"
        );
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn batch_saves_each_item_on_its_own() {
        let mut s = store();
        let body: StoreMemories = serde_json::from_value(json!({ "memories": [
            { "summary": "a" }, { "summary": "" }, { "summary": "c", "namespace": "nope" }, { "summary": "d" }
        ]}))
        .unwrap();
        let r = s
            .store_memories(body.clone(), (1..=4).map(mid).collect(), &ctx(), t(T0))
            .unwrap();
        assert_eq!((r.stored, r.errors), (2, 2));
        let status: Vec<_> = r.results.iter().map(|x| (x.index, x.status)).collect();
        assert_eq!(
            status,
            [
                (0, BatchStatus::Ok),
                (1, BatchStatus::Error),
                (2, BatchStatus::Error),
                (3, BatchStatus::Ok)
            ]
        );
        let e = r.results[1].error.as_ref().unwrap();
        assert_eq!(
            (e.code, e.field.as_deref()),
            (crate::ErrorCode::InvalidArgument, Some("summary"))
        );
        assert_eq!(
            r.results[2].error.as_ref().unwrap().code,
            crate::ErrorCode::NotFound
        );
        assert_eq!(s.changes().content, 2);
        let empty: StoreMemories = serde_json::from_value(json!({ "memories": [] })).unwrap();
        assert_eq!(
            s.store_memories(empty, vec![], &ctx(), t(T0))
                .unwrap_err()
                .field(),
            Some("memories")
        );
        assert!(s.store_memories(body, vec![mid(9)], &ctx(), t(T0)).is_err());
        assert_matches_rebuild(&mut s);
    }

    fn reinforce(s: &mut Store, n: u16, quality: u8, now: i64) -> Result<Reinforced, EngineError> {
        let mut r: ReinforceMemory = serde_json::from_value(json!({ "quality": quality })).unwrap();
        r.id = mid(n);
        s.reinforce_memory(r, &ctx(), t(now))
    }

    #[test]
    fn reinforcement_follows_fsrs_for_each_quality() {
        let elapsed = 8.0;
        let at = T0 + 8 * DAY;
        let mut results = Vec::new();
        for quality in 1..=4 {
            let mut s = store();
            save(&mut s, 1, "alpha");
            let r = reinforce(&mut s, 1, quality, at).unwrap();
            assert_eq!(
                r.stability,
                review_stability(3.7145, elapsed, quality, 1.0, 5.0)
            );
            assert_eq!(
                (r.strength, r.phase, r.is_permastore),
                (1.0, Phase::Full, false)
            );
            let m = s.memory(mid(1)).unwrap();
            assert_eq!((m.decay_anchor_at, m.reinforced_at), (t(at), Some(t(at))));
            assert_eq!(
                m.access_history.last().unwrap().kind,
                AccessKind::Reinforcement
            );
            assert_eq!(m.access_count, 0);
            assert_eq!(s.changes().content, 2);
            results.push(r.stability);
        }
        assert!(results[0] < 3.7145, "a lapse lowers stability");
        assert!(results[1] < results[2] && results[2] < results[3]);
    }

    #[test]
    fn reinforcement_refuses_bad_quality_unknown_ids_and_ghosts_and_sets_permastore() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        for q in [0, 5] {
            assert_eq!(
                reinforce(&mut s, 1, q, T0).unwrap_err().field(),
                Some("quality")
            );
        }
        assert_eq!(
            reinforce(&mut s, 9, 3, T0).unwrap_err().code(),
            crate::ErrorCode::NotFound
        );
        s.update_memory(mid(1), |m| m.stability = 2_000.0);
        let r = reinforce(&mut s, 1, 4, T0 + 3_000 * DAY).unwrap();
        assert!(r.is_permastore && s.namespace_counts(NamespaceId::new(1)).permastore == 1);
        let r = reinforce(&mut s, 1, 1, T0 + 3_001 * DAY).unwrap();
        assert!(r.is_permastore, "permastore is never cleared");
        assert!(s.fade_to_ghost(mid(1), t(T0)));
        let err = reinforce(&mut s, 1, 3, T0).unwrap_err();
        assert_eq!(
            (err.code(), err.to_string().as_str()),
            (crate::ErrorCode::Conflict, "content no longer available")
        );
    }

    fn forget(s: &mut Store, n: u16, ns: Option<&str>) -> bool {
        let req = ForgetMemory {
            id: mid(n),
            namespace: ns.map(str::to_owned),
        };
        s.forget_memory(req, &ctx(), t(T0)).unwrap().deleted
    }

    #[test]
    fn forget_removes_everything_and_is_safe_to_retry() {
        let mut s = store();
        namespace(&mut s, "work");
        s.store_memory(
            req(json!({ "summary": "alpha", "entities": ["Maya"] })),
            mid(1),
            Some(vector("alpha")),
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            req(json!({ "summary": "beta", "entities": ["Maya"] })),
            mid(2),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        assert!(!s.graph().is_empty());
        assert!(
            !forget(&mut s, 1, Some("work")),
            "a namespace mismatch counts as unknown"
        );
        assert!(!forget(&mut s, 1, Some("nope")));
        let content = s.changes().content;
        assert!(forget(&mut s, 1, Some("DEFAULT")));
        assert_eq!(s.changes().content, content + 1);
        assert!(!forget(&mut s, 1, None));
        assert_eq!(s.changes().content, content + 1, "nothing changed");
        assert!(s.memory(mid(1)).is_none() && s.vector(mid(1)).is_none());
        assert_eq!(s.graph().len(), 0);
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn forget_purges_idempotency_entries_that_carry_the_text() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        let entry = |ids| crate::store::IdempotencyEntry {
            request_hash: [0; 32],
            created_at: t(T0),
            response: b"{}".to_vec(),
            text_ids: ids,
        };
        s.insert_idempotency("report".into(), entry(vec![mid(1)]));
        s.insert_idempotency("save".into(), entry(vec![]));
        assert!(forget(&mut s, 1, None));
        assert!(!s.idempotency.contains_key("report"));
        assert!(
            s.idempotency.contains_key("save"),
            "an answer that only names the ID stays"
        );
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
    }

    #[test]
    fn forget_many_reports_each_id_and_dry_runs_change_nothing() {
        let mut s = store();
        save(&mut s, 1, "a");
        save(&mut s, 2, "b");
        let req = |ids: Vec<MemoryId>, dry_run| ForgetMemories { ids, dry_run };
        let r = s
            .forget_memories(req(vec![mid(1), mid(9), mid(1)], true), &ctx(), t(T0))
            .unwrap();
        assert_eq!(
            r.results.iter().map(|x| x.deleted).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(r.deleted, 1);
        assert_eq!(s.memories().count(), 2);
        let r = s
            .forget_memories(req(vec![mid(2), mid(1), mid(1)], false), &ctx(), t(T0))
            .unwrap();
        assert_eq!(
            r.results
                .iter()
                .map(|x| (x.index, x.deleted))
                .collect::<Vec<_>>(),
            [(0, true), (1, true), (2, false)]
        );
        assert_eq!(r.deleted, 2);
        assert_eq!(
            s.forget_memories(req(vec![], false), &ctx(), t(T0))
                .unwrap_err()
                .field(),
            Some("ids")
        );
        assert!(
            s.forget_memories(req(vec![mid(1); 101], false), &ctx(), t(T0))
                .is_err()
        );
    }

    fn mv(s: &mut Store, ids: &[u16], to: &str, from: Option<&str>) -> Result<Moved, EngineError> {
        let req = MoveMemories {
            ids: ids.iter().map(|n| mid(*n)).collect(),
            destination_namespace: to.into(),
            source_namespace: from.map(str::to_owned),
        };
        s.move_memories(req, &ctx(), t(T0))
    }

    #[test]
    fn move_is_all_or_nothing() {
        let mut s = store();
        namespace(&mut s, "work");
        save(&mut s, 1, "alpha");
        save(&mut s, 2, "beta");
        save(&mut s, 3, "gamma");
        s.fade_to_ghost(mid(3), t(T0));
        let before = (s.index_snapshot_for_test(&["alpha"]), s.changes());
        let code = |r: Result<Moved, EngineError>| r.unwrap_err().code();
        assert_eq!(
            code(mv(&mut s, &[1, 9], "work", None)),
            crate::ErrorCode::NotFound
        );
        assert_eq!(
            code(mv(&mut s, &[1, 3], "work", None)),
            crate::ErrorCode::Conflict
        );
        assert_eq!(
            code(mv(&mut s, &[1, 2], "work", Some("work"))),
            crate::ErrorCode::Conflict
        );
        assert_eq!(
            code(mv(&mut s, &[1], "nope", None)),
            crate::ErrorCode::NotFound
        );
        assert_eq!(
            code(mv(&mut s, &[1], "work", Some("nope"))),
            crate::ErrorCode::NotFound
        );
        assert_eq!(
            mv(&mut s, &[1, 1], "work", None).unwrap_err().field(),
            Some("ids")
        );
        assert_eq!((s.index_snapshot_for_test(&["alpha"]), s.changes()), before);
        let r = mv(&mut s, &[1, 2], "WORK", Some("default")).unwrap();
        assert_eq!((r.moved, r.destination_namespace.as_str()), (2, "work"));
        let work = s.resolve_namespace(Some("work")).unwrap().id;
        assert!(
            s.memories()
                .filter(|m| m.id != mid(3))
                .all(|m| m.namespace == work)
        );
        assert_eq!(s.fts().search(work, "alpha", 5).unwrap()[0].0, mid(1));
        assert_eq!(s.changes().content, before.1.content + 1);
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn access_recording_touches_history_but_not_decay() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        save(&mut s, 2, "beta");
        s.fade_to_ghost(mid(2), t(T0));
        let hits = [
            AccessHit {
                id: mid(1),
                kind: AccessKind::Direct,
            },
            AccessHit {
                id: mid(2),
                kind: AccessKind::Direct,
            },
            AccessHit {
                id: mid(9),
                kind: AccessKind::Direct,
            },
        ];
        s.record_access(&hits, t(T0 + DAY));
        s.record_access(&hits[..1], t(T0 + 5));
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(m.retrieved_at, Some(t(T0 + DAY)), "never moves back");
        assert_eq!(m.access_count, 2);
        assert_eq!((m.decay_anchor_at, m.stability), (t(T0), 3.7145));
        assert_eq!(
            s.memory(mid(2)).unwrap().access_count,
            0,
            "ghosts are not recorded"
        );
        assert_eq!(s.changes().bookkeeping, 2);
        s.record_access(&hits[1..], t(T0));
        assert_eq!(
            s.changes().bookkeeping,
            2,
            "nothing recorded, nothing counted"
        );
        for i in 0..40 {
            s.record_access(
                &[AccessHit {
                    id: mid(1),
                    kind: AccessKind::Associative,
                }],
                t(T0 + 2 * DAY + i),
            );
        }
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(m.access_history.len(), 32);
        assert_eq!(m.access_history.last().unwrap().at, t(T0 + 2 * DAY + 39));
        assert_eq!(m.access_count, 42);
    }
}
