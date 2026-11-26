// SPDX-License-Identifier: AGPL-3.0-only
//! Retrieval-induced forgetting: neighbors of what a recall retrieves, activated a
//! moderate amount, rank weaker for that recall. Nothing is saved.
//!
//! Ported from recalld `src/rif/` (activation, plasticity, the engine's
//! `compute_effects`) and `RifProcessorAdapter::compute_suppressions` in
//! `src/search/adapters.rs`. Two differences, both in the spec: a neighbor's strength
//! is computed at `now`, and where two memories share several edges the one with the
//! highest activation counts, not the first recalld added.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::id::MemoryId;
use crate::model::EdgeType;
use crate::model::limits::{RIF_ACTIVATION_HIGH, RIF_ACTIVATION_LOW, RIF_MAX_SUPPRESSION};
use crate::store::Store;
use crate::time::Timestamp;

/// Distance decay per hop; every neighbor here is one hop away.
const GAMMA: f32 = 0.3;
/// The highest strengthening multiplier is `1 + MAX_ENHANCEMENT`.
const MAX_ENHANCEMENT: f32 = 0.05;
/// Neighbors evaluated per retrieved memory (`max_neighbors` in recalld).
const MAX_NEIGHBORS: usize = 100;

/// Edge factor of retrieval-induced forgetting (not the spreading factor).
pub fn rif_edge_factor(edge_type: EdgeType) -> f32 {
    match edge_type {
        EdgeType::Associative => 1.0,
        EdgeType::ParentChild => 0.7,
        EdgeType::Entity => 0.5,
        EdgeType::Temporal => 0.2,
        EdgeType::Supersedes => 0.0,
    }
}

/// How strongly retrieving a memory activates a neighbor one edge away:
/// `weight * 0.3 * R_neighbor * rif_edge_factor`, kept in 0 to 1.
pub fn rif_activation(edge_weight: f32, edge_type: EdgeType, neighbor_strength: f32) -> f32 {
    // recalld's factor order; its similarity factor is always 1.0 on this path.
    let raw = edge_weight * GAMMA * neighbor_strength * rif_edge_factor(edge_type);
    raw.clamp(0.0, 1.0)
}

/// The nonmonotonic plasticity curve: 1.0 below the low bound, a parabolic dip down to
/// `1 - rif_max_suppression` between the bounds, and a linear rise to 1.05 above.
pub fn plasticity_multiplier(activation: f32) -> f32 {
    let (low, high) = (RIF_ACTIVATION_LOW, RIF_ACTIVATION_HIGH);
    if activation < low {
        1.0
    } else if activation < high {
        let center = (low + high) / 2.0;
        let half_width = (high - low) / 2.0;
        let n = (activation - center) / half_width;
        1.0 - RIF_MAX_SUPPRESSION * (1.0 - n * n)
    } else {
        let range = 1.0 - high;
        if range <= 0.0 {
            1.0
        } else {
            1.0 + MAX_ENHANCEMENT * ((activation - high) / range).min(1.0)
        }
    }
}

/// The multipliers below 1.0 for one retrieved memory's neighbors, given as
/// `(neighbor, activation)`: the strongest 100 are evaluated, highest first.
fn suppressions_of(mut neighbors: Vec<(MemoryId, f32)>) -> Vec<(MemoryId, f32)> {
    neighbors.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    neighbors
        .into_iter()
        .take(MAX_NEIGHBORS)
        .filter_map(|(id, activation)| {
            let multiplier = plasticity_multiplier(activation);
            ((multiplier - 1.0).abs() >= f32::EPSILON && multiplier < 1.0)
                .then_some((id, multiplier))
        })
        .collect()
}

impl Store {
    /// The suppressions that retrieving `retrieved` causes at `now`, as
    /// `(target, multiplier)` in order: per retrieved memory, its neighbors by
    /// activation. The caller applies each one to the target when it is a candidate.
    ///
    /// As in recalld, every retrieved memory is offered the neighbor list of all of
    /// them, so a neighbor reached by `k` edges from retrieved memories is evaluated
    /// `k` times for each retrieved memory it shares an edge with.
    pub(crate) fn rif_suppressions(
        &self,
        retrieved: &[MemoryId],
        now: Timestamp,
    ) -> Vec<(MemoryId, f32)> {
        let mut listed: BTreeMap<MemoryId, usize> = BTreeMap::new();
        for &id in retrieved {
            for edge in self.graph.edges_of(id) {
                let count = listed.entry(edge.other(id)).or_default();
                *count = count.saturating_add(1);
            }
        }
        let mut out = Vec::new();
        for &id in retrieved {
            // The best edge to each neighbor, whichever way it points.
            let mut best: BTreeMap<MemoryId, f32> = BTreeMap::new();
            for edge in self.graph.edges_of(id) {
                let other = edge.other(id);
                let Some(neighbor) = self.memories.get(&other) else {
                    continue;
                };
                let strength = self.raw_strength(neighbor, now);
                let activation = rif_activation(edge.weight, edge.edge_type, strength);
                best.entry(other)
                    .and_modify(|a| {
                        if activation.total_cmp(a) == Ordering::Greater {
                            *a = activation;
                        }
                    })
                    .or_insert(activation);
            }
            let mut neighbors = Vec::new();
            for (other, activation) in best {
                let times = listed.get(&other).copied().unwrap_or(0);
                neighbors.extend(std::iter::repeat_n((other, activation), times));
            }
            out.extend(suppressions_of(neighbors));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::id::{NamespaceId, StoreId};
    use crate::model::{Edge, EmbeddingState, Labels, Memory, Phase};
    use crate::testing::FakeProvider;

    fn mid(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    fn memory(n: u8) -> Memory {
        Memory {
            id: mid(n),
            namespace: NamespaceId::new(1),
            created_at: Timestamp::from_millis(0),
            summary: format!("m{n}"),
            full_text: None,
            tags: vec![],
            labels: Labels::default(),
            phase: Phase::Full,
            stability: 3.7145,
            difficulty: 5.0,
            is_permastore: false,
            decay_anchor_at: Timestamp::from_millis(0),
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: Timestamp::from_millis(0),
            access_count: 0,
            access_history: vec![],
            embedding: EmbeddingState::Pending,
            embed_failures: 0,
            saved_by: "local".into(),
            client: None,
        }
    }

    fn edge(s: u8, t: u8, edge_type: EdgeType, weight: f32) -> Edge {
        Edge {
            source: mid(s),
            target: mid(t),
            edge_type,
            weight,
            auto_created: true,
            created_at: Timestamp::from_millis(0),
        }
    }

    fn store(n: u8, edges: &[Edge]) -> Store {
        let mut s = Store::create(
            StoreId::default(),
            FakeProvider::new().identity().clone(),
            Timestamp::from_millis(0),
        );
        for i in 1..=n {
            s.insert_memory(memory(i), None);
        }
        for e in edges {
            s.insert_edge(*e);
        }
        s
    }

    // recalld `rif_edge_factor` table, Knoggin's five types.
    #[test]
    fn edge_factors() {
        assert_eq!(rif_edge_factor(EdgeType::Associative), 1.0);
        assert_eq!(rif_edge_factor(EdgeType::ParentChild), 0.7);
        assert_eq!(rif_edge_factor(EdgeType::Entity), 0.5);
        assert_eq!(rif_edge_factor(EdgeType::Temporal), 0.2);
        assert_eq!(rif_edge_factor(EdgeType::Supersedes), 0.0);
    }

    #[test]
    fn activation_is_the_product_of_its_factors() {
        let a = rif_activation(0.8, EdgeType::Associative, 0.5);
        assert!((a - 0.8 * 0.3 * 0.5).abs() < 1e-7, "{a}");
        assert_eq!(rif_activation(1.0, EdgeType::Supersedes, 1.0), 0.0);
        assert_eq!(rif_activation(1.0, EdgeType::Associative, 1.0), 0.3);
    }

    #[test]
    fn plasticity_curve_has_three_regimes() {
        assert_eq!(plasticity_multiplier(0.0), 1.0);
        assert_eq!(plasticity_multiplier(0.099), 1.0);
        // Edges of the band give (almost) no change, the centre the full dip.
        assert!((plasticity_multiplier(0.1) - 1.0).abs() < 1e-6);
        let center = (RIF_ACTIVATION_LOW + RIF_ACTIVATION_HIGH) / 2.0;
        assert!((plasticity_multiplier(center) - (1.0 - RIF_MAX_SUPPRESSION)).abs() < 1e-6);
        assert!(plasticity_multiplier(0.2) < 1.0 && plasticity_multiplier(0.2) > 0.85);
        assert_eq!(plasticity_multiplier(0.45), 1.0);
        assert!((plasticity_multiplier(1.0) - 1.05).abs() < 1e-6);
        assert!((plasticity_multiplier(2.0) - 1.05).abs() < 1e-6);
        assert!(plasticity_multiplier(0.7) > 1.0 && plasticity_multiplier(0.7) < 1.05);
    }

    #[test]
    fn only_moderately_activated_neighbors_are_suppressed() {
        // 1 retrieved; 2 strong associative (0.3: suppressed), 3 temporal (0.06: none),
        // 4 through a supersedes edge (0: none).
        let s = store(
            4,
            &[
                edge(1, 2, EdgeType::Associative, 1.0),
                edge(3, 1, EdgeType::Temporal, 1.0),
                edge(1, 4, EdgeType::Supersedes, 1.0),
            ],
        );
        let out = s.rif_suppressions(&[mid(1)], Timestamp::from_millis(0));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, mid(2));
        assert!((out[0].1 - plasticity_multiplier(0.3)).abs() < 1e-7);
    }

    #[test]
    fn of_several_edges_the_strongest_counts_whichever_way_it_points() {
        // A temporal edge 1 -> 2 (0.06, no effect) and an associative edge 2 -> 1 (0.3).
        let s = store(
            2,
            &[
                edge(1, 2, EdgeType::Temporal, 1.0),
                edge(2, 1, EdgeType::Associative, 1.0),
            ],
        );
        let out = s.rif_suppressions(&[mid(1)], Timestamp::from_millis(0));
        // 2 is listed twice (two edges touch it), so it is evaluated twice.
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(id, m)| *id == mid(2) && *m < 1.0));
    }

    #[test]
    fn only_the_hundred_most_activated_neighbors_are_evaluated() {
        // 1 retrieved; 100 neighbors at weight 1.0 (activation 0.3) and one more, the
        // smallest ID, at weight 0.5 (0.15). Both would be suppressed if evaluated.
        let mut edges = vec![edge(1, 2, EdgeType::Associative, 0.5)];
        edges.extend((3..=102).map(|n| edge(1, n, EdgeType::Associative, 1.0)));
        let s = store(102, &edges);
        assert!(plasticity_multiplier(0.15) < 1.0);
        let out = s.rif_suppressions(&[mid(1)], Timestamp::from_millis(0));
        assert_eq!(out.len(), MAX_NEIGHBORS);
        assert!(
            out.iter().all(|(id, _)| *id != mid(2)),
            "the 101st by activation is not evaluated"
        );
    }

    #[test]
    fn a_neighbor_shared_by_two_retrieved_memories_is_listed_for_each() {
        // 3 is a neighbor of both 1 and 2.
        let s = store(
            3,
            &[
                edge(1, 3, EdgeType::Associative, 1.0),
                edge(2, 3, EdgeType::Associative, 1.0),
            ],
        );
        let out = s.rif_suppressions(&[mid(1), mid(2)], Timestamp::from_millis(0));
        assert_eq!(
            out.len(),
            4,
            "listed twice, for each of two retrieved memories"
        );
        assert!(out.iter().all(|(id, _)| *id == mid(3)));
    }
}
