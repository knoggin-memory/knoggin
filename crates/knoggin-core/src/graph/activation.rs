// SPDX-License-Identifier: AGPL-3.0-only
//! Priority-queue spreading activation for recall's graph expansion.
//!
//! Ported from recalld `spreading_activation_resolved` in `src/graph/activation.rs`
//! (lines 563 to 792), with its tests. Strengths are computed at `now`; ties in the
//! queue and in the output go to the smaller ID, where recalld left them to its heap
//! and hash map.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use crate::graph::{fan_strength, spreading_edge_factor};
use crate::id::{MemoryId, NamespaceId};
use crate::model::Phase;
use crate::ops::{TraceSeed, TraceSpread};
use crate::store::Store;
use crate::time::Timestamp;

/// Spreads below this are dropped.
const MIN_SPREAD: f32 = 0.005;

/// The budget of one `depth` (recalld `spreading_config`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct SpreadConfig {
    max_budget: usize,
    hop_decay: f32,
    firing_threshold: f32,
    output_threshold: f32,
}

fn spread_config(depth: u8) -> SpreadConfig {
    match depth {
        2 => SpreadConfig {
            max_budget: 200,
            hop_decay: 0.75,
            firing_threshold: 0.02,
            output_threshold: 0.015,
        },
        3 => SpreadConfig {
            max_budget: 400,
            hop_decay: 0.85,
            firing_threshold: 0.015,
            output_threshold: 0.01,
        },
        _ => SpreadConfig {
            max_budget: 100,
            hop_decay: 0.75,
            firing_threshold: 0.03,
            output_threshold: 0.02,
        },
    }
}

/// What spreading found, and how.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Spread {
    /// Memories that are not seeds, at or above the output threshold, by activation
    /// descending, then ID.
    pub(crate) activated: Vec<(MemoryId, f32)>,
    /// The seeds that started, with their starting activation.
    pub(crate) seeds: Vec<TraceSeed>,
    /// Every accumulation, in firing order.
    pub(crate) spreads: Vec<TraceSpread>,
}

/// A queue entry: highest activation first, then the smaller ID.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Entry {
    activation: f32,
    id: MemoryId,
}

impl Eq for Entry {}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.activation
            .total_cmp(&other.activation)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Store {
    /// Raw strength at `now` of a memory that may take part in spreading: in
    /// `namespace` and not a ghost.
    fn live_strength(&self, id: MemoryId, namespace: NamespaceId, now: Timestamp) -> Option<f32> {
        let memory = self.memories.get(&id)?;
        (memory.namespace == namespace && memory.phase != Phase::Ghost)
            .then(|| self.raw_strength(memory, now))
    }

    /// Spreads activation from `seeds` (`(id, relevance)`) through the graph of
    /// `namespace` under the budget of `depth` (1 to 3).
    ///
    /// A seed starts at `clamp(relevance * R, 0, 1)`. The node with the highest
    /// activation fires once: along every edge with a non-zero factor, to a live
    /// neighbor in the namespace, it adds `activation * weight * edge_factor * fan *
    /// hop_decay * R_neighbor`, capped at 1.0, where `fan` comes from the firing node's
    /// degree.
    pub(crate) fn spreading_activation(
        &self,
        seeds: &[(MemoryId, f32)],
        namespace: NamespaceId,
        depth: u8,
        now: Timestamp,
    ) -> Spread {
        let config = spread_config(depth);
        let mut strengths: BTreeMap<MemoryId, Option<f32>> = BTreeMap::new();
        let mut strength = |id: MemoryId| {
            *strengths
                .entry(id)
                .or_insert_with(|| self.live_strength(id, namespace, now))
        };
        let mut activation: BTreeMap<MemoryId, f32> = BTreeMap::new();
        let mut fired: BTreeSet<MemoryId> = BTreeSet::new();
        let mut seed_ids: BTreeSet<MemoryId> = BTreeSet::new();
        let mut queue: BinaryHeap<Entry> = BinaryHeap::new();
        let mut out = Spread::default();

        for &(id, relevance) in seeds {
            let Some(r) = strength(id) else {
                continue;
            };
            let initial = (relevance * r).clamp(0.0, 1.0);
            if initial < config.firing_threshold {
                continue;
            }
            activation.insert(id, initial);
            seed_ids.insert(id);
            out.seeds.push(TraceSeed { id, score: initial });
            queue.push(Entry {
                activation: initial,
                id,
            });
        }

        let mut processed: usize = 0;
        while let Some(entry) = queue.pop() {
            if processed >= config.max_budget {
                break;
            }
            if fired.contains(&entry.id) {
                continue;
            }
            let Some(&current) = activation.get(&entry.id) else {
                continue;
            };
            if current < config.firing_threshold || strength(entry.id).is_none() {
                continue;
            }
            fired.insert(entry.id);
            processed = processed.saturating_add(1);
            let order = u32::try_from(processed.saturating_sub(1)).unwrap_or(u32::MAX);

            let fan = fan_strength(self.graph.degree(entry.id));
            if fan == 0.0 {
                continue;
            }
            for edge in self.graph.edges_of(entry.id) {
                let factor = spreading_edge_factor(edge.edge_type);
                if factor == 0.0 {
                    continue;
                }
                let neighbor = edge.other(entry.id);
                let Some(r) = strength(neighbor) else {
                    continue;
                };
                let spread = current * edge.weight * factor * fan * config.hop_decay * r;
                if spread < MIN_SPREAD {
                    continue;
                }
                let slot = activation.entry(neighbor).or_insert(0.0);
                let new = (*slot + spread).min(1.0);
                *slot = new;
                out.spreads.push(TraceSpread {
                    from: entry.id,
                    to: neighbor,
                    amount: spread,
                    edge_type: edge.edge_type,
                    order,
                });
                if new >= config.firing_threshold && !fired.contains(&neighbor) {
                    queue.push(Entry {
                        activation: new,
                        id: neighbor,
                    });
                }
            }
        }

        out.activated = activation
            .into_iter()
            .filter(|(id, act)| !seed_ids.contains(id) && *act >= config.output_threshold)
            .collect();
        out.activated
            .sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::EmbeddingProvider;
    use crate::id::StoreId;
    use crate::model::{
        Edge, EdgeType, EmbeddingState, Labels, Memory, Namespace, NamespacePolicy,
    };
    use crate::testing::FakeProvider;

    const NS: NamespaceId = NamespaceId::new(1);
    const OTHER: NamespaceId = NamespaceId::new(2);
    const NOW: Timestamp = Timestamp::from_millis(0);

    /// A store in which every memory has strength 1.0 at `NOW`.
    struct Graph {
        store: Store,
        next: u8,
    }

    impl Graph {
        fn new() -> Self {
            let mut store = Store::create(
                StoreId::default(),
                FakeProvider::new().identity().clone(),
                NOW,
            );
            store.insert_namespace(Namespace {
                id: OTHER,
                name: "other".into(),
                created_at: NOW,
                policy: NamespacePolicy::default(),
            });
            Self { store, next: 0 }
        }

        fn node(&mut self, namespace: NamespaceId) -> MemoryId {
            self.next += 1;
            let mut b = [0; 16];
            b[15] = self.next;
            let id = MemoryId::from_bytes(b);
            self.store.insert_memory(
                Memory {
                    id,
                    namespace,
                    created_at: NOW,
                    summary: "m".into(),
                    full_text: None,
                    tags: vec![],
                    labels: Labels::default(),
                    phase: Phase::Full,
                    stability: 3.7145,
                    difficulty: 5.0,
                    is_permastore: false,
                    decay_anchor_at: NOW,
                    retrieved_at: None,
                    reinforced_at: None,
                    phase_changed_at: NOW,
                    access_count: 0,
                    access_history: vec![],
                    embedding: EmbeddingState::Pending,
                    embed_failures: 0,
                    saved_by: "local".into(),
                    client: None,
                },
                None,
            );
            id
        }

        fn link(&mut self, source: MemoryId, target: MemoryId, edge_type: EdgeType) {
            assert!(self.store.insert_edge(Edge {
                source,
                target,
                edge_type,
                weight: 1.0,
                auto_created: false,
                created_at: NOW,
            }));
        }

        fn spread(&self, seeds: &[(MemoryId, f32)]) -> Vec<(MemoryId, f32)> {
            self.store.spreading_activation(seeds, NS, 1, NOW).activated
        }
    }

    fn act(results: &[(MemoryId, f32)], id: MemoryId) -> Option<f32> {
        results.iter().find(|(m, _)| *m == id).map(|(_, a)| *a)
    }

    #[test]
    fn test_spreading_basic() {
        let mut g = Graph::new();
        let (a, b, c) = (g.node(NS), g.node(NS), g.node(NS));
        g.link(a, b, EdgeType::Associative);
        g.link(b, c, EdgeType::Associative);
        let results = g.spread(&[(a, 1.0)]);
        let (b_act, c_act) = (act(&results, b).unwrap(), act(&results, c).unwrap());
        assert!(b_act > c_act, "{b_act} vs {c_act}");
        assert!(act(&results, a).is_none(), "seeds are not returned");
        // b keeps gathering after it fires: c spreads back into it, capped at 1.0.
        assert!(b_act > fan_strength(1) * 0.75 && b_act <= 1.0);
    }

    #[test]
    fn test_spreading_multi_seed() {
        let mut g = Graph::new();
        let (a, b, c, d, e) = (g.node(NS), g.node(NS), g.node(NS), g.node(NS), g.node(NS));
        g.link(a, c, EdgeType::Associative);
        g.link(b, c, EdgeType::Associative);
        g.link(a, d, EdgeType::Associative);
        g.link(b, e, EdgeType::Associative);
        let results = g.spread(&[(a, 1.0), (b, 1.0)]);
        let c_act = act(&results, c).unwrap();
        assert!(c_act > act(&results, d).unwrap(), "convergence");
        assert!(c_act > act(&results, e).unwrap(), "convergence");
    }

    #[test]
    fn test_spreading_fan_attenuation() {
        let mut g = Graph::new();
        let (a, hub, beyond) = (g.node(NS), g.node(NS), g.node(NS));
        g.link(a, hub, EdgeType::Associative);
        g.link(hub, beyond, EdgeType::Associative);
        for _ in 0..6 {
            let extra = g.node(NS);
            g.link(hub, extra, EdgeType::Associative);
        }
        assert!(g.store.graph().degree(hub) >= 7);
        let results = g.spread(&[(a, 1.0)]);
        assert!(act(&results, hub).is_some(), "a spreads into the hub");
        assert!(
            act(&results, beyond).is_none(),
            "a hub of fan 0 spreads nothing"
        );
    }

    #[test]
    fn test_spreading_supersedes_blocked() {
        // recalld's test is for `Contradicts`; Knoggin's zero-factor type is `Supersedes`.
        let mut g = Graph::new();
        let (a, b) = (g.node(NS), g.node(NS));
        g.link(a, b, EdgeType::Supersedes);
        assert!(act(&g.spread(&[(a, 1.0)]), b).is_none());
    }

    #[test]
    fn test_spreading_decay_stops() {
        let mut g = Graph::new();
        let chain: Vec<MemoryId> = (0..20).map(|_| g.node(NS)).collect();
        for pair in chain.windows(2) {
            g.link(pair[0], pair[1], EdgeType::Associative);
        }
        let results = g.spread(&[(chain[0], 1.0)]);
        for &far in &chain[15..] {
            assert!(act(&results, far).is_none(), "signal decayed");
        }
        assert!(act(&results, chain[1]).is_some());
    }

    #[test]
    fn test_spreading_namespace_isolation() {
        let mut g = Graph::new();
        let (a, b, c) = (g.node(NS), g.node(OTHER), g.node(NS));
        g.link(a, b, EdgeType::Associative);
        g.link(a, c, EdgeType::Associative);
        let results = g.spread(&[(a, 1.0)]);
        assert!(act(&results, b).is_none());
        assert!(act(&results, c).is_some());
        // A seed outside the namespace does not start.
        assert!(
            g.store
                .spreading_activation(&[(b, 1.0)], NS, 1, NOW)
                .seeds
                .is_empty()
        );
    }

    #[test]
    fn test_spreading_ghost_skipped() {
        let mut g = Graph::new();
        let (a, ghost, c) = (g.node(NS), g.node(NS), g.node(NS));
        g.link(a, ghost, EdgeType::Associative);
        g.link(ghost, c, EdgeType::Associative);
        g.store.fade_to_ghost(ghost, NOW);
        let results = g.spread(&[(a, 1.0)]);
        assert!(act(&results, ghost).is_none());
        assert!(act(&results, c).is_none(), "a ghost does not relay");
    }

    #[test]
    fn terminal_nodes_neither_seed_nor_relay_retained_structural_edges() {
        let mut g = Graph::new();
        let (first, middle, last) = (g.node(NS), g.node(NS), g.node(NS));
        g.link(first, middle, EdgeType::Supersedes);
        g.link(first, middle, EdgeType::Associative);
        g.link(middle, last, EdgeType::ParentChild);
        assert!(act(&g.spread(&[(first, 1.0)]), last).is_some());
        g.store.fade_to_ghost(middle, NOW);
        assert!(g.spread(&[(middle, 1.0)]).is_empty());
        assert!(g.spread(&[(first, 1.0)]).is_empty());
        assert_eq!(g.store.graph().len(), 3, "edges stay");
    }

    #[test]
    fn weak_seeds_do_not_start_and_depth_picks_the_budget() {
        let mut g = Graph::new();
        let (a, b) = (g.node(NS), g.node(NS));
        g.link(a, b, EdgeType::Associative);
        // 0.02 is below the depth-1 firing threshold (0.03) but not depth 3's (0.015).
        assert!(
            g.store
                .spreading_activation(&[(a, 0.02)], NS, 1, NOW)
                .seeds
                .is_empty()
        );
        let deep = g.store.spreading_activation(&[(a, 0.02)], NS, 3, NOW);
        assert_eq!(deep.seeds, [TraceSeed { id: a, score: 0.02 }]);
        // a fires into b, then b fires back into a (which has already fired).
        assert_eq!(deep.spreads.len(), 2);
        let spread = deep.spreads[0];
        assert_eq!((spread.from, spread.to, spread.order), (a, b, 0));
        let back = deep.spreads[1];
        assert_eq!((back.from, back.to, back.order), (b, a, 1));
        assert_eq!(spread.edge_type, EdgeType::Associative);
        assert_eq!(spread.amount, 0.02 * fan_strength(1) * 0.85);
        assert_eq!(deep.activated, [(b, spread.amount)]);
    }

    #[test]
    fn entries_order_by_activation_then_smaller_id() {
        let lo = Entry {
            activation: 0.5,
            id: MemoryId::from_bytes([2; 16]),
        };
        let hi = Entry {
            activation: 0.5,
            id: MemoryId::from_bytes([1; 16]),
        };
        let mut heap = BinaryHeap::from([lo, hi]);
        assert_eq!(heap.pop(), Some(hi));
    }
}
