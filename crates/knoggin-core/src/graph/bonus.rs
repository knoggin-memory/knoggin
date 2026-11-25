// SPDX-License-Identifier: AGPL-3.0-only
//! Strength, the connection bonus and effective strength.
//!
//! Ported from recalld `src/graph/activation.rs` (lines 151 to 361:
//! `connection_bonus_with_strengths`, `collect_neighbor_contributions`, `fan_strength`)
//! and `src/search/strength.rs`. Knoggin computes every neighbor's strength at `now`;
//! recalld read a value stored at the last sweep.

use std::collections::{BTreeMap, BTreeSet};

use crate::decay::fsrs::{effective_retrievability, retrievability};
use crate::id::{MemoryId, NamespaceId};
use crate::model::limits::MAX_CONNECTION_BONUS;
use crate::model::{EdgeType, Memory, Phase};
use crate::store::Store;
use crate::time::{Timestamp, elapsed_days};

/// ACT-R maximum associative strength. At a fan of 7 or more a neighbor gives nothing.
const S_MAX: f32 = 2.0;
/// Scale of the one-hop contributions.
const SCALE: f32 = 0.05;
/// Extra factor of the two-hop contributions.
const HOP_2_DECAY: f32 = 0.5;

/// Edge factor of the connection bonus and of spreading activation.
pub fn spreading_edge_factor(edge_type: EdgeType) -> f32 {
    match edge_type {
        EdgeType::Associative => 1.0,
        EdgeType::ParentChild => 0.7,
        EdgeType::Entity | EdgeType::Temporal => 0.6,
        EdgeType::Supersedes => 0.0,
    }
}

/// ACT-R association strength of a neighbor with `fan` edges: `max(0, 2.0 - ln(fan + 1))`.
pub fn fan_strength(fan: usize) -> f32 {
    (S_MAX - (fan as f32 + 1.0).ln()).max(0.0)
}

impl Store {
    /// Raw strength of a memory at `now`: the forgetting curve from its decay anchor,
    /// with its namespace's decay-rate multiplier.
    pub fn raw_strength(&self, memory: &Memory, now: Timestamp) -> f32 {
        let multiplier = self
            .namespaces
            .get(&memory.namespace)
            .map_or(1.0, |n| n.policy.multiplier());
        retrievability(
            elapsed_days(now, memory.decay_anchor_at),
            memory.stability,
            multiplier,
        )
    }

    /// Raw and effective strength of a memory at `now`. A ghost has no bonus.
    pub fn strengths(&self, memory: &Memory, now: Timestamp) -> (f32, f32) {
        let raw = self.raw_strength(memory, now);
        if memory.phase == Phase::Ghost {
            return (raw, raw);
        }
        let bonus = self.connection_bonus(memory.id, now);
        (raw, effective_retrievability(raw, bonus))
    }

    /// Effective strength `R + bonus * (1 - R)` of one memory at `now`, for the recall
    /// pipeline and the sweep alike.
    pub fn effective_strength(&self, id: MemoryId, now: Timestamp) -> Option<f32> {
        let memory = self.memories.get(&id)?;
        Some(self.strengths(memory, now).1)
    }

    /// The connection bonus of one memory at `now`, 0 to 0.15.
    ///
    /// One edge away, in either direction: `fan_strength(fan_j) * R_j * weight *
    /// edge_factor`, summed, times 0.05. Two edges away (not the memory itself, not a
    /// one-edge neighbor): the largest such product over paths, times 0.05 * 0.5. Only
    /// neighbors in the memory's namespace that are not ghosts count, and only they relay.
    pub fn connection_bonus(&self, id: MemoryId, now: Timestamp) -> f32 {
        let Some(memory) = self.memories.get(&id) else {
            return 0.0;
        };
        let namespace = memory.namespace;
        let hop1 = self.neighbor_contributions(id, namespace, now);
        let mut bonus = 0.0_f32;
        for (_, contribution) in &hop1 {
            bonus += contribution * SCALE;
        }
        let hop1_ids: BTreeSet<MemoryId> = hop1.iter().map(|(n, _)| *n).collect();
        let mut hop2_best: BTreeMap<MemoryId, f32> = BTreeMap::new();
        for (neighbor, _) in &hop1 {
            for (far, contribution) in self.neighbor_contributions(*neighbor, namespace, now) {
                if far == id || hop1_ids.contains(&far) {
                    continue;
                }
                let best = hop2_best.entry(far).or_insert(0.0);
                *best = best.max(contribution);
            }
        }
        for contribution in hop2_best.values() {
            bonus += contribution * SCALE * HOP_2_DECAY;
        }
        bonus.min(MAX_CONNECTION_BONUS)
    }

    /// One `(neighbor, contribution)` per edge of `id` with a non-zero factor, to a
    /// neighbor in `namespace` that is not a ghost and has a fan strength above 0.
    /// Sorted by neighbor ID, then contribution, as recalld sorts them.
    fn neighbor_contributions(
        &self,
        id: MemoryId,
        namespace: NamespaceId,
        now: Timestamp,
    ) -> Vec<(MemoryId, f32)> {
        let mut out = Vec::new();
        for edge in self.graph.edges_of(id) {
            let factor = spreading_edge_factor(edge.edge_type);
            if factor == 0.0 {
                continue;
            }
            let other = edge.other(id);
            let Some(neighbor) = self.memories.get(&other) else {
                continue;
            };
            if neighbor.namespace != namespace || neighbor.phase == Phase::Ghost {
                continue;
            }
            let s_ji = fan_strength(self.graph.degree(other));
            if s_ji > 0.0 {
                let strength = self.raw_strength(neighbor, now);
                out.push((other, s_ji * strength * edge.weight * factor));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::id::StoreId;
    use crate::model::{Edge, EmbeddingState, Labels, Namespace, NamespacePolicy};
    use crate::testing::FakeProvider;

    const DAY: i64 = 86_400_000;

    fn mid(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    fn t(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    fn memory(n: u8, ns: NamespaceId, anchor: i64) -> Memory {
        Memory {
            id: mid(n),
            namespace: ns,
            created_at: t(anchor),
            summary: format!("m{n}"),
            full_text: None,
            tags: vec![],
            labels: Labels::default(),
            phase: Phase::Full,
            stability: 3.7145,
            difficulty: 5.0,
            is_permastore: false,
            decay_anchor_at: t(anchor),
            retrieved_at: None,
            reinforced_at: None,
            phase_changed_at: t(anchor),
            access_count: 0,
            access_history: vec![],
            embedding: EmbeddingState::Pending,
            embed_failures: 0,
            saved_by: "local".into(),
            client: None,
        }
    }

    fn edge(s: u8, t_: u8, edge_type: EdgeType, weight: f32) -> Edge {
        Edge {
            source: mid(s),
            target: mid(t_),
            edge_type,
            weight,
            auto_created: true,
            created_at: t(0),
        }
    }

    fn store() -> Store {
        Store::create(
            StoreId::default(),
            FakeProvider::new().identity().clone(),
            t(0),
        )
    }

    const NS: NamespaceId = NamespaceId::new(1);

    #[test]
    fn edge_factors_and_fan() {
        assert_eq!(spreading_edge_factor(EdgeType::Associative), 1.0);
        assert_eq!(spreading_edge_factor(EdgeType::ParentChild), 0.7);
        assert_eq!(spreading_edge_factor(EdgeType::Entity), 0.6);
        assert_eq!(spreading_edge_factor(EdgeType::Temporal), 0.6);
        assert_eq!(spreading_edge_factor(EdgeType::Supersedes), 0.0);
        assert_eq!(fan_strength(1), 2.0 - 2.0_f32.ln());
        assert!(fan_strength(6) > 0.0);
        assert_eq!(fan_strength(7), 0.0, "a hub of 7 edges gives nothing");
        assert_eq!(fan_strength(1000), 0.0);
    }

    #[test]
    fn raw_strength_follows_the_curve_and_the_multiplier() {
        let mut s = store();
        s.insert_memory(memory(1, NS, 0), None);
        let m = s.memory(mid(1)).unwrap().clone();
        assert_eq!(s.raw_strength(&m, t(0)), 1.0);
        let r = s.raw_strength(&m, t(4 * DAY));
        assert!(r < 0.9 && r > 0.85, "{r}");
        assert_eq!(
            s.raw_strength(&m, t(-DAY)),
            1.0,
            "a time in the future counts as now"
        );
        s.namespaces
            .get_mut(&NS)
            .unwrap()
            .policy
            .decay_rate_multiplier = Some(0.0);
        assert_eq!(s.raw_strength(&m, t(400 * DAY)), 1.0);
    }

    /// A chain 1 - 2 - 3 plus a supersedes edge 1 -> 4 and a ghost 5 linked to 1.
    #[test]
    fn bonus_counts_one_and_two_hops_and_skips_what_does_not_count() {
        let mut s = store();
        for n in 1..=5 {
            s.insert_memory(memory(n, NS, 0), None);
        }
        s.insert_edge(edge(1, 2, EdgeType::Associative, 0.8));
        s.insert_edge(edge(3, 2, EdgeType::Entity, 0.5));
        s.insert_edge(edge(1, 4, EdgeType::Supersedes, 1.0));
        s.insert_edge(edge(5, 1, EdgeType::Associative, 1.0));
        s.fade_to_ghost(mid(5), t(0));
        let now = t(0); // every R is 1.
        // One hop: only 2 (4 is Supersedes, 5 a ghost). Fan of 2 is 2 edges.
        let hop1 = fan_strength(2) * 0.8 * 1.0;
        // Two hops: 3 through 2. Fan of 3 is 1 edge.
        let hop2 = fan_strength(1) * 0.5 * 0.6;
        let expected = hop1 * 0.05 + hop2 * 0.05 * 0.5;
        let got = s.connection_bonus(mid(1), now);
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
        let (raw, eff) = s.strengths(s.memory(mid(1)).unwrap(), now);
        assert_eq!(raw, 1.0);
        assert_eq!(eff, 1.0);
        let later = t(30 * DAY);
        let (raw, eff) = s.strengths(s.memory(mid(1)).unwrap(), later);
        let bonus = s.connection_bonus(mid(1), later);
        assert!(bonus > 0.0);
        assert_eq!(eff, effective_retrievability(raw, bonus));
        assert_eq!(s.effective_strength(mid(1), later), Some(eff));
        assert_eq!(s.effective_strength(mid(99), later), None);
        // A ghost has no bonus.
        let ghost = s.memory(mid(5)).unwrap();
        let (raw, eff) = s.strengths(ghost, later);
        assert_eq!(raw, eff);
    }

    #[test]
    fn neighbors_in_another_namespace_do_not_count_and_bonus_is_capped() {
        let mut s = store();
        let other = NamespaceId::new(2);
        s.insert_namespace(Namespace {
            id: other,
            name: "other".into(),
            created_at: t(0),
            policy: NamespacePolicy::default(),
        });
        s.insert_memory(memory(1, NS, 0), None);
        s.insert_memory(memory(2, other, 0), None);
        s.insert_edge(edge(1, 2, EdgeType::Associative, 1.0));
        assert_eq!(s.connection_bonus(mid(1), t(0)), 0.0);

        // Many strong one-edge neighbors: the sum passes 0.15 and is capped.
        let mut s = store();
        s.insert_memory(memory(1, NS, 0), None);
        for n in 2..=6 {
            s.insert_memory(memory(n, NS, 0), None);
            s.insert_edge(edge(1, n, EdgeType::Associative, 1.0));
        }
        let raw_sum = 5.0 * fan_strength(1) * 0.05;
        assert!(raw_sum > MAX_CONNECTION_BONUS);
        assert_eq!(s.connection_bonus(mid(1), t(0)), MAX_CONNECTION_BONUS);
    }

    #[test]
    fn two_hop_takes_the_best_path_once() {
        let mut s = store();
        for n in 1..=4 {
            s.insert_memory(memory(n, NS, 0), None);
        }
        // 1 - 2 - 4 (weight 0.2) and 1 - 3 - 4 (weight 0.9): 4 counts once, at 0.9.
        s.insert_edge(edge(1, 2, EdgeType::Associative, 1.0));
        s.insert_edge(edge(1, 3, EdgeType::Associative, 1.0));
        s.insert_edge(edge(2, 4, EdgeType::Associative, 0.2));
        s.insert_edge(edge(3, 4, EdgeType::Associative, 0.9));
        let hop1 = 2.0 * fan_strength(2) * 1.0;
        let hop2 = fan_strength(2) * 0.9;
        let expected = (hop1 * 0.05 + hop2 * 0.05 * 0.5).min(MAX_CONNECTION_BONUS);
        let got = s.connection_bonus(mid(1), t(0));
        assert!((got - expected).abs() < 1e-6, "{got} vs {expected}");
    }
}
