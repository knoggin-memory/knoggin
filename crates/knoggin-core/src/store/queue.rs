// SPDX-License-Identifier: AGPL-3.0-only
//! The engine side of the embedding queue, the model change and `on_open`.

use crate::embed::{ModelIdentity, document_text};
use crate::id::MemoryId;
use crate::model::limits::{IDEMPOTENCY_TTL_MS, MAX_EMBED_FAILURES};
use crate::model::{EdgeType, EmbeddingState, Phase};
use crate::ops::{ApplyOutcome, EmbedJob, SkipReason};
use crate::search::vector::is_valid_vector;
use crate::time::Timestamp;

use super::Store;

impl Store {
    fn bump_bookkeeping(&mut self) {
        self.changes.bookkeeping = self.changes.bookkeeping.saturating_add(1);
    }

    /// Up to `max` jobs of the queue, newest first by `created_at`, then the higher ID.
    pub fn pending_embeddings(&self, max: usize) -> Vec<EmbedJob> {
        // ponytail: a full scan per call; keep a pending set if the budget test says so.
        let mut pending: Vec<_> = self
            .memories
            .values()
            .filter(|m| m.embedding == EmbeddingState::Pending)
            .collect();
        pending.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        pending
            .into_iter()
            .take(max)
            .map(|m| EmbedJob {
                id: m.id,
                text: document_text(m),
            })
            .collect()
    }

    /// Stores the vector of a `Pending` memory from the store's current model and adds
    /// its automatic semantic links. Anything else is skipped; a vector that is not
    /// valid counts as a failure.
    pub fn apply_embedding(
        &mut self,
        id: MemoryId,
        model: &ModelIdentity,
        vector: Vec<f32>,
        now: Timestamp,
    ) -> ApplyOutcome {
        let Some(memory) = self.memories.get(&id) else {
            return ApplyOutcome::Skipped(SkipReason::Missing);
        };
        if memory.embedding != EmbeddingState::Pending {
            return ApplyOutcome::Skipped(SkipReason::NotPending);
        }
        if *model != self.model {
            return ApplyOutcome::Skipped(SkipReason::ModelMismatch);
        }
        if !is_valid_vector(&vector, self.vectors.dimensions()) || !self.attach_vector(id, &vector)
        {
            self.fail_embedding(id);
            return ApplyOutcome::Skipped(SkipReason::InvalidVector);
        }
        let links = self.add_semantic_links(id, now);
        self.bump_bookkeeping();
        ApplyOutcome::Applied { links }
    }

    /// Counts a failed embedding of a `Pending` memory; after three it is `Failed` and
    /// leaves the queue until the store is next opened. Any other memory is left alone.
    pub fn fail_embedding(&mut self, id: MemoryId) {
        let counted = self.update_memory(id, |m| {
            if m.embedding != EmbeddingState::Pending {
                return false;
            }
            m.embed_failures = m.embed_failures.saturating_add(1);
            if m.embed_failures >= MAX_EMBED_FAILURES {
                m.embedding = EmbeddingState::Failed;
            }
            true
        });
        if counted == Some(true) {
            self.bump_bookkeeping();
        }
    }

    /// Takes a new embedding model: drops every vector and every automatic semantic
    /// link, and queues every memory that still has text.
    pub fn adopt_model(&mut self, model: ModelIdentity) {
        self.remove_edges_where(|e| e.auto_created && e.edge_type == EdgeType::Associative);
        self.reset_vectors(model);
        self.bump_bookkeeping();
    }

    /// Called once after a store is decoded: queues `Failed` memories again with their
    /// count at 0 and drops idempotency entries older than 24 hours.
    pub fn on_open(&mut self, now: Timestamp) {
        let failed: Vec<MemoryId> = self
            .memories
            .values()
            .filter(|m| m.embedding == EmbeddingState::Failed && m.phase != Phase::Ghost)
            .map(|m| m.id)
            .collect();
        let expired: Vec<String> = self
            .idempotency
            .iter()
            .filter(|(_, e)| is_expired(e.created_at, now))
            .map(|(k, _)| k.clone())
            .collect();
        for id in &failed {
            self.update_memory(*id, |m| {
                m.embedding = EmbeddingState::Pending;
                m.embed_failures = 0;
            });
        }
        for key in &expired {
            self.remove_idempotency(key);
        }
        if !failed.is_empty() || !expired.is_empty() {
            self.bump_bookkeeping();
        }
    }
}

/// An idempotency entry made at `created_at` is older than 24 hours at `now`.
pub(crate) fn is_expired(created_at: Timestamp, now: Timestamp) -> bool {
    now.millis().saturating_sub(created_at.millis()) > IDEMPOTENCY_TTL_MS
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::model::Edge;
    use crate::store::IdempotencyEntry;
    use crate::store::write::tests::{
        DAY, T0, assert_matches_rebuild, ctx, mid, req, save, store, t, vector,
    };
    use crate::testing::FakeProvider;

    fn model() -> ModelIdentity {
        FakeProvider::new().identity().clone()
    }

    #[test]
    fn jobs_come_newest_first_then_higher_id() {
        let mut s = store();
        for (n, at) in [
            (1, "2023-01-02T00:00:00Z"),
            (2, "2023-01-01T00:00:00Z"),
            (3, "2023-01-02T00:00:00Z"),
        ] {
            s.store_memory(
                req(json!({ "summary": format!("m{n}"), "createdAt": at })),
                mid(n),
                None,
                &ctx(),
                t(T0),
            )
            .unwrap();
        }
        save(&mut s, 4, "ready");
        s.apply_embedding(mid(4), &model(), vector("ready"), t(T0));
        let ids: Vec<_> = s.pending_embeddings(10).iter().map(|j| j.id).collect();
        assert_eq!(ids, [mid(3), mid(1), mid(2)]);
        assert_eq!(s.pending_embeddings(1).len(), 1);
        let job = &s.pending_embeddings(1)[0];
        assert_eq!(job.text, document_text(s.memory(mid(3)).unwrap()));
    }

    #[test]
    fn apply_checks_state_model_and_vector_then_links() {
        let mut s = store();
        save(&mut s, 1, "maya likes window seats");
        save(&mut s, 2, "maya likes window seats");
        let before = s.changes().bookkeeping;
        assert_eq!(
            s.apply_embedding(mid(9), &model(), vector("x"), t(T0)),
            ApplyOutcome::Skipped(SkipReason::Missing)
        );
        let other = FakeProvider::with_dim(32).identity().clone();
        assert_eq!(
            s.apply_embedding(mid(1), &other, vector("x"), t(T0)),
            ApplyOutcome::Skipped(SkipReason::ModelMismatch)
        );
        assert_eq!(s.changes().bookkeeping, before);
        assert_eq!(
            s.apply_embedding(mid(1), &model(), vec![0.5; 64], t(T0)),
            ApplyOutcome::Skipped(SkipReason::InvalidVector)
        );
        assert_eq!(
            s.memory(mid(1)).unwrap().embed_failures,
            1,
            "an invalid vector counts as a failure"
        );
        let ApplyOutcome::Applied { links } =
            s.apply_embedding(mid(1), &model(), vector("maya likes window seats"), t(T0))
        else {
            panic!("not applied");
        };
        assert!(links.is_empty(), "no other memory has a vector yet");
        assert_eq!(s.memory(mid(1)).unwrap().embed_failures, 0);
        assert_eq!(
            s.apply_embedding(mid(1), &model(), vector("x"), t(T0)),
            ApplyOutcome::Skipped(SkipReason::NotPending)
        );
        let ApplyOutcome::Applied { links } = s.apply_embedding(
            mid(2),
            &model(),
            vector("maya likes window seats"),
            t(T0 + 1),
        ) else {
            panic!("not applied");
        };
        assert_eq!(links.len(), 1);
        let e = links[0];
        assert_eq!(
            (e.source, e.target, e.edge_type, e.auto_created),
            (mid(2), mid(1), EdgeType::Associative, true)
        );
        assert!((e.weight - 1.0).abs() < 1e-5);
        assert_eq!(e.created_at, t(T0 + 1));
        assert_eq!(s.changes().bookkeeping, before + 3);
        // A ghost that was pending gets no vector.
        save(&mut s, 3, "c");
        s.fade_to_ghost(mid(3), t(T0));
        assert_eq!(
            s.apply_embedding(mid(3), &model(), vector("c"), t(T0)),
            ApplyOutcome::Skipped(SkipReason::NotPending)
        );
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn three_failures_leave_the_queue_until_the_store_opens_again() {
        let mut s = store();
        save(&mut s, 1, "a");
        save(&mut s, 2, "b");
        s.apply_embedding(mid(2), &model(), vector("b"), t(T0));
        let before = s.changes().bookkeeping;
        s.fail_embedding(mid(2));
        s.fail_embedding(mid(9));
        assert_eq!(
            s.changes().bookkeeping,
            before,
            "only pending memories count"
        );
        for _ in 0..2 {
            s.fail_embedding(mid(1));
        }
        assert_eq!(s.memory(mid(1)).unwrap().embedding, EmbeddingState::Pending);
        s.fail_embedding(mid(1));
        assert_eq!(s.memory(mid(1)).unwrap().embedding, EmbeddingState::Failed);
        assert!(s.pending_embeddings(5).is_empty());
        s.fail_embedding(mid(1));
        assert_eq!(s.memory(mid(1)).unwrap().embed_failures, 3);
        assert_eq!(s.changes().bookkeeping, before + 3);
        s.on_open(t(T0));
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(
            (m.embedding, m.embed_failures),
            (EmbeddingState::Pending, 0)
        );
        assert_eq!(s.changes().bookkeeping, before + 4);
        s.on_open(t(T0));
        assert_eq!(
            s.changes().bookkeeping,
            before + 4,
            "nothing to do, nothing counted"
        );
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn on_open_drops_entries_older_than_a_day() {
        let mut s = store();
        let entry = |at| IdempotencyEntry {
            request_hash: [0; 32],
            created_at: t(at),
            response: vec![],
            text_ids: vec![],
        };
        s.insert_idempotency("old".into(), entry(T0));
        s.insert_idempotency("edge".into(), entry(T0 + 1));
        s.on_open(t(T0 + DAY + 1));
        assert!(!s.idempotency.contains_key("old"));
        assert!(
            s.idempotency.contains_key("edge"),
            "exactly 24 hours old is kept"
        );
        assert_eq!(s.estimated_bytes(), s.estimate_from_scratch());
    }

    #[test]
    fn adopt_model_drops_vectors_and_automatic_semantic_links_only() {
        let mut s = store();
        let text = "maya likes window seats";
        s.store_memory(
            req(json!({ "summary": text })),
            mid(1),
            Some(vector(text)),
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.store_memory(
            req(json!({ "summary": text, "parentId": mid(1) })),
            mid(2),
            Some(vector(text)),
            &ctx(),
            t(T0),
        )
        .unwrap();
        save(&mut s, 3, "ghost");
        s.fade_to_ghost(mid(3), t(T0));
        s.insert_edge(Edge {
            source: mid(1),
            target: mid(2),
            edge_type: EdgeType::Associative,
            weight: 0.9,
            auto_created: false,
            created_at: t(T0),
        });
        let types = |s: &Store| {
            s.graph()
                .iter()
                .map(|e| (e.edge_type, e.auto_created))
                .collect::<Vec<_>>()
        };
        assert!(types(&s).contains(&(EdgeType::Associative, true)));
        let new = FakeProvider::with_dim(32);
        s.adopt_model(new.identity().clone());
        assert!(!types(&s).contains(&(EdgeType::Associative, true)));
        assert!(
            types(&s).contains(&(EdgeType::Associative, false)),
            "a manual edge stays"
        );
        assert!(types(&s).contains(&(EdgeType::ParentChild, false)));
        assert!(s.vectors().is_empty());
        assert_eq!(s.memory(mid(3)).unwrap().embedding, EmbeddingState::None);
        let ids: Vec<_> = s.pending_embeddings(5).iter().map(|j| j.id).collect();
        assert_eq!(ids, [mid(2), mid(1)]);
        assert!(matches!(
            s.apply_embedding(mid(1), &new.identity().clone(), new.vector(text), t(T0)),
            ApplyOutcome::Applied { .. }
        ));
        assert_eq!(s.changes().bookkeeping, 2);
        assert_matches_rebuild(&mut s);
    }
}
