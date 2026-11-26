// SPDX-License-Identifier: AGPL-3.0-only
//! The tag index: per namespace, each plain tag with the memories that carry it.
//! Label terms are not in it; ghosts have no tags.

use std::collections::{BTreeMap, BTreeSet};

use crate::id::{MemoryId, NamespaceId};
use crate::model::Tag;

/// Per namespace, tag to memory IDs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TagIndex {
    index: BTreeMap<NamespaceId, BTreeMap<Tag, BTreeSet<MemoryId>>>,
}

impl std::fmt::Debug for TagIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagIndex")
            .field("namespaces", &self.index.len())
            .finish()
    }
}

impl TagIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one memory under each of its tags.
    pub fn add(&mut self, namespace: NamespaceId, memory: MemoryId, tags: &[Tag]) {
        if tags.is_empty() {
            return;
        }
        let by_tag = self.index.entry(namespace).or_default();
        for tag in tags {
            by_tag.entry(tag.clone()).or_default().insert(memory);
        }
    }

    /// Removes one memory from each of its tags.
    pub fn remove(&mut self, namespace: NamespaceId, memory: MemoryId, tags: &[Tag]) {
        let Some(by_tag) = self.index.get_mut(&namespace) else {
            return;
        };
        for tag in tags {
            if let Some(ids) = by_tag.get_mut(tag) {
                ids.remove(&memory);
                if ids.is_empty() {
                    by_tag.remove(tag);
                }
            }
        }
        if by_tag.is_empty() {
            self.index.remove(&namespace);
        }
    }

    /// Every tag of `namespace` with its number of memories, in tag order.
    pub fn counts(&self, namespace: NamespaceId) -> impl Iterator<Item = (&Tag, u64)> {
        self.index
            .get(&namespace)
            .into_iter()
            .flat_map(|by_tag| by_tag.iter().map(|(t, ids)| (t, ids.len() as u64)))
    }

    /// Every `(namespace, tag, memories)`, in order. For index comparisons.
    pub fn entries(&self) -> impl Iterator<Item = (NamespaceId, &Tag, &BTreeSet<MemoryId>)> {
        self.index
            .iter()
            .flat_map(|(ns, by_tag)| by_tag.iter().map(move |(t, ids)| (*ns, t, ids)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    fn tags(names: &[&str]) -> Vec<Tag> {
        names.iter().map(|n| Tag::new(*n).unwrap()).collect()
    }

    #[test]
    fn add_query_remove() {
        let ns = NamespaceId::new(1);
        let other = NamespaceId::new(2);
        let mut idx = TagIndex::new();
        idx.add(ns, mid(2), &tags(&["a", "b"]));
        idx.add(ns, mid(1), &tags(&["a"]));
        idx.add(other, mid(3), &tags(&["a"]));
        let a = Tag::new("a").unwrap();
        let holders: Vec<_> = idx.entries().filter(|e| e.0 == ns && *e.1 == a).collect();
        assert_eq!(holders.len(), 1);
        assert_eq!(
            holders[0].2.iter().copied().collect::<Vec<_>>(),
            [mid(1), mid(2)]
        );
        let counts: Vec<_> = idx
            .counts(ns)
            .map(|(t, c)| (t.as_str().to_owned(), c))
            .collect();
        assert_eq!(counts, [("a".to_owned(), 2), ("b".to_owned(), 1)]);
        idx.remove(ns, mid(2), &tags(&["a", "b"]));
        idx.remove(ns, mid(1), &tags(&["a"]));
        assert_eq!(idx.counts(ns).count(), 0);
        assert_eq!(idx.entries().count(), 1);
        idx.remove(other, mid(3), &tags(&["a"]));
        assert_eq!(idx, TagIndex::new());
    }
}
