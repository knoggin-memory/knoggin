// SPDX-License-Identifier: AGPL-3.0-only
//! Automatic links and the duplicate report.
//!
//! Ported from recalld `src/graph/native_selection.rs` (semantic and entity
//! selection, the duplicate advisory) and `src/storage/native_store.rs`
//! (`select_temporal_links`). Entity and temporal links are chosen at save time;
//! semantic links when the memory's vector lands, with every memory of the namespace
//! that has a vector then as a candidate.

use std::collections::BTreeSet;

use crate::id::MemoryId;
use crate::model::limits::{
    AUTO_LINK_SHARED_TAG_THRESHOLD, AUTO_LINK_THRESHOLD, DUPLICATE_THRESHOLD, MAX_AUTO_LINKS,
    MAX_ENTITY_LINKS, MAX_NEAR_DUPLICATES, MAX_TEMPORAL_LINKS, MIN_TEMPORAL_WEIGHT,
    NEAR_DUPLICATE_SUMMARY_MAX_BYTES, TEMPORAL_WINDOW_MS, truncate_summary,
};
use crate::model::{Edge, EdgeType, Memory, Phase};
use crate::ops::{NearDuplicate, NearDuplicates};
use crate::search::entity::canonicalize_entity;
use crate::search::vector::dot;
use crate::store::Store;
use crate::time::Timestamp;

/// recalld `autolink::THRESHOLD_HARD_FLOOR`: no semantic link below this cosine.
const SEMANTIC_FLOOR: f32 = 0.40;

/// Every canonical key of a list of entities, last-word keys included.
fn entity_keys(entities: &[String]) -> BTreeSet<String> {
    entities
        .iter()
        .flat_map(|e| canonicalize_entity(e))
        .collect()
}

/// A cosine kept in range: bound vectors can round a dot product just outside it.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    dot(a, b).clamp(-1.0, 1.0)
}

/// `Full` or `Summary`: the phases that take part in links and duplicate reports.
fn has_content(m: &Memory) -> bool {
    matches!(m.phase, Phase::Full | Phase::Summary)
}

/// Entity links: candidates sharing at least one canonical key, by shared count
/// descending then ID, weight shared / union, at most `MAX_ENTITY_LINKS`.
pub(crate) fn select_entity_links<'a>(
    entities: &[String],
    candidates: impl Iterator<Item = &'a Memory>,
) -> Vec<(MemoryId, f32)> {
    let new_keys = entity_keys(entities);
    if new_keys.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(MemoryId, usize, usize)> = candidates
        .filter_map(|c| {
            let keys = entity_keys(&c.labels.entities);
            let shared = new_keys.intersection(&keys).count();
            (shared > 0).then(|| (c.id, shared, new_keys.union(&keys).count()))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(MAX_ENTITY_LINKS)
        .map(|(id, shared, union)| (id, shared as f32 / union as f32))
        .collect()
}

/// Temporal links: candidates whose creation time is within the window, by distance
/// then ID, weight `max(1 - distance / window, 0.05)`, at most `MAX_TEMPORAL_LINKS`.
pub(crate) fn select_temporal_links(
    candidates: &[(MemoryId, i64)],
    created_at: i64,
) -> Vec<(MemoryId, f32)> {
    let window = TEMPORAL_WINDOW_MS.unsigned_abs();
    let mut selected: Vec<(MemoryId, u64)> = candidates
        .iter()
        .filter_map(|&(id, at)| {
            let delta = created_at.abs_diff(at);
            (delta <= window).then_some((id, delta))
        })
        .collect();
    selected.sort_by_key(|&(id, delta)| (delta, id));
    selected
        .into_iter()
        .take(MAX_TEMPORAL_LINKS)
        .map(|(id, delta)| {
            let weight = (1.0 - delta as f64 / window as f64).max(f64::from(MIN_TEMPORAL_WEIGHT));
            (id, weight as f32)
        })
        .collect()
}

/// Semantic links from scored candidates: cosine at least 0.50, or 0.45 for one
/// that shares a tag term, by cosine descending then ID, at most `MAX_AUTO_LINKS`.
pub(crate) fn select_semantic_links(
    mut scored: Vec<(MemoryId, f32, bool)>,
) -> Vec<(MemoryId, f32)> {
    let threshold = AUTO_LINK_THRESHOLD.max(SEMANTIC_FLOOR);
    let shared_threshold = AUTO_LINK_SHARED_TAG_THRESHOLD.max(SEMANTIC_FLOOR);
    scored.retain(|&(_, score, shared)| score >= if shared { shared_threshold } else { threshold });
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
        .into_iter()
        .take(MAX_AUTO_LINKS)
        .map(|(id, score, _)| (id, score))
        .collect()
}

fn auto_edge(
    source: MemoryId,
    target: MemoryId,
    edge_type: EdgeType,
    weight: f32,
    now: Timestamp,
) -> Edge {
    Edge {
        source,
        target,
        edge_type,
        weight,
        auto_created: true,
        created_at: now,
    }
}

impl Store {
    /// The other memories of `memory`'s namespace that links may join.
    fn link_candidates<'a>(&'a self, memory: &'a Memory) -> impl Iterator<Item = &'a Memory> + 'a {
        self.memories
            .values()
            .filter(move |c| c.id != memory.id && c.namespace == memory.namespace && has_content(c))
    }

    /// Adds the entity and temporal links of a memory that was just saved. Returns
    /// the edges made.
    pub(crate) fn add_save_links(&mut self, id: MemoryId, now: Timestamp) -> Vec<Edge> {
        let Some(memory) = self.memories.get(&id) else {
            return Vec::new();
        };
        let entity = select_entity_links(&memory.labels.entities, self.link_candidates(memory));
        let times: Vec<(MemoryId, i64)> = self
            .link_candidates(memory)
            .map(|c| (c.id, c.created_at.millis()))
            .collect();
        let temporal = select_temporal_links(&times, memory.created_at.millis());
        let planned: Vec<Edge> = entity
            .into_iter()
            .map(|(t, w)| auto_edge(id, t, EdgeType::Entity, w, now))
            .chain(
                temporal
                    .into_iter()
                    .map(|(t, w)| auto_edge(id, t, EdgeType::Temporal, w, now)),
            )
            .collect();
        self.insert_new_links(planned)
    }

    /// Adds the semantic links of a memory whose vector just landed: every other
    /// memory of its namespace with a vector is a candidate. Returns the edges made.
    pub(crate) fn add_semantic_links(&mut self, id: MemoryId, now: Timestamp) -> Vec<Edge> {
        let (Some(memory), Some(vector)) = (self.memories.get(&id), self.vectors.get(id)) else {
            return Vec::new();
        };
        let terms: BTreeSet<String> = memory.tag_terms().into_iter().collect();
        let scored: Vec<(MemoryId, f32, bool)> = self
            .link_candidates(memory)
            .filter_map(|c| {
                let other = self.vectors.get(c.id)?;
                let shared = c.tag_terms().iter().any(|t| terms.contains(t));
                Some((c.id, cosine(vector, other), shared))
            })
            .collect();
        let planned = select_semantic_links(scored)
            .into_iter()
            .map(|(t, w)| auto_edge(id, t, EdgeType::Associative, w, now))
            .collect();
        self.insert_new_links(planned)
    }

    /// Inserts planned links, leaving out any pair that an edge of the same type
    /// already joins in either direction.
    fn insert_new_links(&mut self, planned: Vec<Edge>) -> Vec<Edge> {
        planned
            .into_iter()
            .filter(|e| !self.graph.joined(e.source, e.target, e.edge_type) && self.insert_edge(*e))
            .collect()
    }

    /// The near-duplicate report for a new memory's vector in `namespace`, before the
    /// memory is inserted. `exclude` is left out (the memory a supersede replaces).
    pub(crate) fn duplicate_report(
        &self,
        namespace: crate::id::NamespaceId,
        vector: &[f32],
        exclude: Option<MemoryId>,
    ) -> Option<NearDuplicates> {
        let mut matches: Vec<(MemoryId, f32)> = self
            .memories
            .values()
            .filter(|m| m.namespace == namespace && has_content(m) && Some(m.id) != exclude)
            .filter_map(|m| Some((m.id, cosine(vector, self.vectors.get(m.id)?))))
            .filter(|&(_, score)| score >= DUPLICATE_THRESHOLD)
            .collect();
        matches.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        matches.truncate(MAX_NEAR_DUPLICATES);
        if matches.is_empty() {
            return None;
        }
        Some(NearDuplicates {
            threshold: DUPLICATE_THRESHOLD,
            matches: matches
                .into_iter()
                .map(|(id, score)| NearDuplicate {
                    id,
                    score,
                    summary: self
                        .memories
                        .get(&id)
                        .map(|m| truncate_summary(&m.summary, NEAR_DUPLICATE_SUMMARY_MAX_BYTES))
                        .unwrap_or_default(),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: u8) -> MemoryId {
        MemoryId::from_bytes([n; 16])
    }

    // Ported from recalld `native_store_advisory_disabled_checked_empty_and_temporal_uuid_ties`.
    #[test]
    fn temporal_ties_go_to_the_smaller_id_and_extremes_do_not_overflow() {
        let (small, large) = (mid(1), mid(2));
        assert_eq!(
            select_temporal_links(&[(large, 1100), (small, 900)], 1000)[0].0,
            small
        );
        assert_eq!(
            select_temporal_links(&[(small, 900), (large, 1100)], 1000)[0].0,
            small
        );
        assert!(select_temporal_links(&[(small, i64::MIN)], i64::MAX).is_empty());
    }

    #[test]
    fn temporal_weight_falls_with_distance_to_a_floor_and_caps_at_twenty() {
        let w = select_temporal_links(&[(mid(1), 0), (mid(2), 1_800_000), (mid(3), 3_600_000)], 0);
        assert_eq!(w, [(mid(1), 1.0), (mid(2), 0.5), (mid(3), 0.05)]);
        assert!(select_temporal_links(&[(mid(1), 3_600_001)], 0).is_empty());
        let many: Vec<_> = (1..=30).map(|n| (mid(n), i64::from(n))).collect();
        let picked = select_temporal_links(&many, 0);
        assert_eq!(picked.len(), MAX_TEMPORAL_LINKS);
        assert_eq!(picked[0].0, mid(1));
        assert_eq!(picked[19].0, mid(20));
    }

    #[test]
    fn semantic_threshold_is_lower_for_shared_tags_and_caps_at_fifteen() {
        let picked = select_semantic_links(vec![
            (mid(1), 0.49, false),
            (mid(2), 0.49, true),
            (mid(3), 0.44, true),
            (mid(4), 0.50, false),
            (mid(5), 0.90, false),
            (mid(6), 0.90, false),
        ]);
        assert_eq!(
            picked,
            [
                (mid(5), 0.90),
                (mid(6), 0.90),
                (mid(4), 0.50),
                (mid(2), 0.49)
            ]
        );
        let many: Vec<_> = (1..=20).map(|n| (mid(n), 0.6, false)).collect();
        let picked = select_semantic_links(many);
        assert_eq!(picked.len(), MAX_AUTO_LINKS);
        assert_eq!(picked.last().unwrap().0, mid(15));
    }

    fn with_entities(n: u8, entities: &[&str]) -> Memory {
        Memory {
            id: mid(n),
            namespace: crate::id::NamespaceId::new(1),
            created_at: Timestamp::from_millis(0),
            summary: "s".into(),
            full_text: None,
            tags: Vec::new(),
            labels: crate::model::Labels {
                entities: entities.iter().map(|s| (*s).to_owned()).collect(),
                ..Default::default()
            },
            phase: Phase::Full,
            stability: 1.0,
            difficulty: 5.0,
            is_permastore: false,
            decay_anchor_at: Timestamp::from_millis(0),
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: Timestamp::from_millis(0),
            access_count: 0,
            access_history: Vec::new(),
            embedding: crate::model::EmbeddingState::Pending,
            embed_failures: 0,
            saved_by: "local".into(),
            client: None,
        }
    }

    #[test]
    fn entity_links_order_by_shared_keys_and_weigh_by_jaccard() {
        let a = with_entities(1, &["Maya Smith"]);
        let b = with_entities(2, &["Smith"]);
        let c = with_entities(3, &["Lisbon"]);
        let d = with_entities(4, &["Maya Smith", "Lisbon"]);
        let picked = select_entity_links(&["Maya Smith".to_owned()], [&a, &b, &c, &d].into_iter());
        // New keys {maya smith, smith}. a: 2/2. d: 2/3. b: 1/2.
        assert_eq!(picked, [(mid(1), 1.0), (mid(4), 2.0 / 3.0), (mid(2), 0.5)]);
        assert!(select_entity_links(&[], [&a].into_iter()).is_empty());
        let many: Vec<Memory> = (10..30).map(|n| with_entities(n, &["Lisbon"])).collect();
        let picked = select_entity_links(&["Lisbon".to_owned()], many.iter());
        assert_eq!(picked.len(), MAX_ENTITY_LINKS);
        assert_eq!(picked[0].0, mid(10));
    }
}
