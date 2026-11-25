// SPDX-License-Identifier: AGPL-3.0-only
//! The in-memory graph: edges keyed by `(source, edge_type, target)` and an adjacency
//! list per memory. Also the connection bonus and effective strength.

mod activation;
mod bonus;
mod links;

use std::collections::{BTreeMap, BTreeSet};

use crate::id::MemoryId;
use crate::model::{Edge, EdgeKey, EdgeType};

pub use bonus::{fan_strength, spreading_edge_factor};

/// Every edge of a store, with the edges of each memory in both directions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Graph {
    edges: BTreeMap<EdgeKey, Edge>,
    adjacency: BTreeMap<MemoryId, BTreeSet<EdgeKey>>,
}

impl Graph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an edge. Returns false, changing nothing, when an edge with the same key
    /// exists or the edge is a loop.
    pub fn insert(&mut self, edge: Edge) -> bool {
        let key = edge.key();
        if edge.source == edge.target || self.edges.contains_key(&key) {
            return false;
        }
        self.edges.insert(key, edge);
        self.adjacency.entry(edge.source).or_default().insert(key);
        self.adjacency.entry(edge.target).or_default().insert(key);
        true
    }

    /// Removes one edge.
    pub fn remove(&mut self, key: &EdgeKey) -> Option<Edge> {
        let edge = self.edges.remove(key)?;
        for end in [edge.source, edge.target] {
            if let Some(keys) = self.adjacency.get_mut(&end) {
                keys.remove(key);
                if keys.is_empty() {
                    self.adjacency.remove(&end);
                }
            }
        }
        Some(edge)
    }

    /// Removes every edge that touches `memory`, in key order.
    pub fn remove_node(&mut self, memory: MemoryId) -> Vec<Edge> {
        let keys: Vec<EdgeKey> = self
            .adjacency
            .get(&memory)
            .map(|k| k.iter().copied().collect())
            .unwrap_or_default();
        keys.iter().filter_map(|k| self.remove(k)).collect()
    }

    /// Removes every edge for which `drop` holds, in key order. Returns them.
    pub fn remove_where(&mut self, mut drop: impl FnMut(&Edge) -> bool) -> Vec<Edge> {
        let keys: Vec<EdgeKey> = self
            .edges
            .values()
            .filter(|e| drop(e))
            .map(Edge::key)
            .collect();
        keys.iter().filter_map(|k| self.remove(k)).collect()
    }

    /// The edge with this key.
    pub fn get(&self, key: &EdgeKey) -> Option<&Edge> {
        self.edges.get(key)
    }

    /// Whether an edge with this key exists.
    pub fn contains(&self, key: &EdgeKey) -> bool {
        self.edges.contains_key(key)
    }

    /// Whether an edge of `edge_type` joins `a` and `b`, in either direction.
    pub fn joined(&self, a: MemoryId, b: MemoryId, edge_type: EdgeType) -> bool {
        let key = |source, target| EdgeKey {
            source,
            edge_type,
            target,
        };
        self.edges.contains_key(&key(a, b)) || self.edges.contains_key(&key(b, a))
    }

    /// The edges of `memory` in both directions, in key order.
    pub fn edges_of(&self, memory: MemoryId) -> impl Iterator<Item = &Edge> {
        self.adjacency
            .get(&memory)
            .into_iter()
            .flatten()
            .filter_map(|k| self.edges.get(k))
    }

    /// Edges stored for `memory`, of every type, whatever is at the other end. This
    /// is `degree` in spreading activation, `fan` in the connection bonus and
    /// `edgeCount` in the memory view.
    pub fn degree(&self, memory: MemoryId) -> usize {
        self.adjacency.get(&memory).map_or(0, BTreeSet::len)
    }

    /// Every edge in key order.
    pub fn iter(&self) -> impl Iterator<Item = &Edge> {
        self.edges.values()
    }

    /// Number of edges.
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// No edges.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// The adjacency lists, for index comparisons.
    pub fn adjacency(&self) -> &BTreeMap<MemoryId, BTreeSet<EdgeKey>> {
        &self.adjacency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::Timestamp;

    fn mid(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    fn edge(s: u8, t: u8, edge_type: EdgeType) -> Edge {
        Edge {
            source: mid(s),
            target: mid(t),
            edge_type,
            weight: 0.5,
            auto_created: true,
            created_at: Timestamp::from_millis(0),
        }
    }

    #[test]
    fn keys_are_unique_and_loops_refused() {
        let mut g = Graph::new();
        assert!(g.insert(edge(1, 2, EdgeType::Entity)));
        assert!(!g.insert(edge(1, 2, EdgeType::Entity)));
        assert!(g.insert(edge(2, 1, EdgeType::Entity)));
        assert!(g.insert(edge(1, 2, EdgeType::Temporal)));
        assert!(!g.insert(edge(3, 3, EdgeType::Entity)));
        assert_eq!(g.len(), 3);
        assert_eq!(g.degree(mid(1)), 3);
        assert!(g.joined(mid(2), mid(1), EdgeType::Temporal));
        assert!(!g.joined(mid(2), mid(1), EdgeType::Associative));
    }

    #[test]
    fn removal_keeps_adjacency_consistent() {
        let mut g = Graph::new();
        g.insert(edge(1, 2, EdgeType::Entity));
        g.insert(edge(3, 1, EdgeType::Associative));
        g.insert(edge(2, 3, EdgeType::Temporal));
        let removed = g.remove_node(mid(1));
        assert_eq!(removed.len(), 2);
        assert_eq!(g.degree(mid(1)), 0);
        assert_eq!(g.degree(mid(2)), 1);
        assert!(!g.adjacency().contains_key(&mid(1)));
        let others: Vec<_> = g.edges_of(mid(3)).map(|e| e.other(mid(3))).collect();
        assert_eq!(others, [mid(2)]);
        let dropped = g.remove_where(|e| e.edge_type == EdgeType::Temporal);
        assert_eq!(dropped.len(), 1);
        assert!(g.is_empty());
        assert!(g.adjacency().is_empty());
    }
}
