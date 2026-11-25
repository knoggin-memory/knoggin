// SPDX-License-Identifier: AGPL-3.0-only
//! Entity normalization and the entity index. Ported exactly, with the honorific list
//! and the tests, from recalld `src/search/entity_normalize.rs` and `entity_index.rs`.
//! Ordered collections replace recalld's hash maps.

use std::collections::{BTreeMap, BTreeSet};

use crate::id::{MemoryId, NamespaceId};

/// Leading honorifics, removed once. Matched as plain prefixes, as recalld does.
const HONORIFICS: &[&str] = &[
    "dr.",
    "dr",
    "mr.",
    "mr",
    "mrs.",
    "mrs",
    "ms.",
    "ms",
    "professor",
    "prof.",
    "prof",
    "captain",
    "capt.",
    "capt",
    "officer",
    "sir",
    "dame",
    "lord",
    "lady",
    "reverend",
    "rev.",
    "rev",
    "sergeant",
    "sgt.",
    "sgt",
    "lieutenant",
    "lt.",
    "lt",
    "corporal",
    "cpl.",
    "cpl",
    "private",
    "pvt.",
    "pvt",
    "general",
    "gen.",
    "gen",
    "colonel",
    "col.",
    "col",
    "major",
    "maj.",
    "maj",
    "judge",
    "justice",
    "senator",
    "representative",
    "congressman",
    "congresswoman",
    "president",
    "governor",
    "mayor",
    "coach",
    "chef",
    "nurse",
    "brother",
    "sister",
    "father",
    "mother",
    "uncle",
    "aunt",
];

/// The canonical keys of an entity name: the normalized full name, and for a name of
/// several words also its last word. Empty for a blank name.
///
/// Trim, lower-case, remove a trailing possessive, remove one leading honorific,
/// collapse white space.
pub fn canonicalize_entity(raw: &str) -> Vec<String> {
    let mut name = raw.trim().to_lowercase();
    // recalld's list holds the ASCII form twice; the curly form is the other one.
    for suffix in ["'s", "\u{2019}s", "'s"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.trim_end().to_owned();
            break;
        }
    }
    for prefix in HONORIFICS {
        if let Some(rest) = name.strip_prefix(prefix) {
            let rest = rest.trim_start();
            if !rest.is_empty() {
                name = rest.to_owned();
                break;
            }
        }
    }
    let parts: Vec<&str> = name.split_whitespace().collect();
    let Some(last) = parts.last().map(|s| (*s).to_owned()) else {
        return Vec::new();
    };
    let full = parts.join(" ");
    let several = parts.len() >= 2;
    let mut keys = vec![full];
    if several && last.len() > 1 && keys.first() != Some(&last) {
        keys.push(last);
    }
    keys
}

/// Jaccard overlap of the first canonical key of each entity in the two lists; 0 when
/// either is empty. recalld `model::tag::entity_overlap`.
pub fn entity_overlap(query_entities: &[String], memory_entities: &[String]) -> f32 {
    if query_entities.is_empty() || memory_entities.is_empty() {
        return 0.0;
    }
    let first_keys = |list: &[String]| -> BTreeSet<String> {
        list.iter()
            .filter_map(|e| canonicalize_entity(e).into_iter().next())
            .collect()
    };
    let q = first_keys(query_entities);
    let m = first_keys(memory_entities);
    let union = q.union(&m).count();
    if union == 0 {
        return 0.0;
    }
    q.intersection(&m).count() as f32 / union as f32
}

/// Per namespace, a map from canonical entity key to memory IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityIndex {
    index: BTreeMap<NamespaceId, BTreeMap<String, BTreeSet<MemoryId>>>,
    keys_by_memory: BTreeMap<(NamespaceId, MemoryId), BTreeSet<String>>,
}

impl EntityIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the postings of one memory with those of `entities`. Idempotent;
    /// an empty (or wholly blank) list removes the old postings.
    pub fn add(&mut self, namespace: NamespaceId, memory: MemoryId, entities: &[String]) {
        let keys: BTreeSet<String> = entities
            .iter()
            .flat_map(|e| canonicalize_entity(e))
            .collect();
        if self.keys_by_memory.get(&(namespace, memory)) == Some(&keys) {
            return;
        }
        self.remove(namespace, memory);
        if keys.is_empty() {
            return;
        }
        let postings = self.index.entry(namespace).or_default();
        for key in &keys {
            postings.entry(key.clone()).or_default().insert(memory);
        }
        self.keys_by_memory.insert((namespace, memory), keys);
    }

    /// Removes every posting of one memory. Returns whether it had any.
    pub fn remove(&mut self, namespace: NamespaceId, memory: MemoryId) -> bool {
        let Some(keys) = self.keys_by_memory.remove(&(namespace, memory)) else {
            return false;
        };
        if let Some(postings) = self.index.get_mut(&namespace) {
            for key in keys {
                if let Some(ids) = postings.get_mut(&key) {
                    ids.remove(&memory);
                    if ids.is_empty() {
                        postings.remove(&key);
                    }
                }
            }
            if postings.is_empty() {
                self.index.remove(&namespace);
            }
        }
        true
    }

    /// Removes a whole namespace.
    pub fn remove_namespace(&mut self, namespace: NamespaceId) {
        self.index.remove(&namespace);
        self.keys_by_memory.retain(|(ns, _), _| *ns != namespace);
    }

    /// Memories of `namespace` sharing canonical keys with `entities`, by number of
    /// shared keys descending, then ID. A key asked for twice counts once.
    pub fn find_by_entities(
        &self,
        namespace: NamespaceId,
        entities: &[String],
        exclude: MemoryId,
    ) -> Vec<(MemoryId, usize)> {
        self.find_by_entities_filtered(namespace, entities, exclude, |_| true)
    }

    /// As [`Self::find_by_entities`], counting only memories for which `eligible` holds.
    pub fn find_by_entities_filtered(
        &self,
        namespace: NamespaceId,
        entities: &[String],
        exclude: MemoryId,
        mut eligible: impl FnMut(MemoryId) -> bool,
    ) -> Vec<(MemoryId, usize)> {
        let Some(postings) = self.index.get(&namespace) else {
            return Vec::new();
        };
        let mut counts: BTreeMap<MemoryId, usize> = BTreeMap::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for entity in entities {
            for key in canonicalize_entity(entity) {
                if !seen.insert(key.clone()) {
                    continue;
                }
                if let Some(ids) = postings.get(&key) {
                    for &id in ids {
                        if id != exclude && eligible(id) {
                            *counts.entry(id).or_default() += 1;
                        }
                    }
                }
            }
        }
        let mut results: Vec<_> = counts.into_iter().collect();
        results.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        results
    }

    /// The canonical keys indexed for one memory.
    pub fn keys_of(&self, namespace: NamespaceId, memory: MemoryId) -> Option<&BTreeSet<String>> {
        self.keys_by_memory.get(&(namespace, memory))
    }

    /// Memories with at least one key.
    pub fn len(&self) -> usize {
        self.keys_by_memory.len()
    }

    /// No memory has a key.
    pub fn is_empty(&self) -> bool {
        self.keys_by_memory.is_empty()
    }

    /// Every `(namespace, memory)` with its keys, in order. For index comparisons.
    pub fn entries(&self) -> impl Iterator<Item = (&(NamespaceId, MemoryId), &BTreeSet<String>)> {
        self.keys_by_memory.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── entity_normalize.rs ─────────────────────────────────────────

    #[test]
    fn basic_case_fold() {
        assert_eq!(canonicalize_entity("Alice"), vec!["alice"]);
    }

    #[test]
    fn honorific_stripping() {
        assert_eq!(
            canonicalize_entity("Dr. Sarah Martinez"),
            vec!["sarah martinez", "martinez"]
        );
    }

    #[test]
    fn possessive_stripping() {
        assert_eq!(canonicalize_entity("Alice's"), vec!["alice"]);
        assert_eq!(canonicalize_entity("Alice\u{2019}s"), vec!["alice"]);
    }

    #[test]
    fn multi_word_last_name() {
        assert_eq!(
            canonicalize_entity("John Smith"),
            vec!["john smith", "smith"]
        );
    }

    #[test]
    fn whitespace_collapse() {
        assert_eq!(
            canonicalize_entity("  Dr.   Sarah   Martinez  "),
            vec!["sarah martinez", "martinez"]
        );
    }

    #[test]
    fn empty_input() {
        assert!(canonicalize_entity("").is_empty());
        assert!(canonicalize_entity("   ").is_empty());
    }

    #[test]
    fn full_word_honorifics() {
        assert_eq!(
            canonicalize_entity("Professor Dumbledore"),
            vec!["dumbledore"]
        );
        assert_eq!(canonicalize_entity("Reverend Smith"), vec!["smith"]);
        assert_eq!(canonicalize_entity("General Patton"), vec!["patton"]);
        assert_eq!(canonicalize_entity("Colonel Sanders"), vec!["sanders"]);
        assert_eq!(canonicalize_entity("Sergeant York"), vec!["york"]);
    }

    /// Ported as recalld has it: honorifics match as plain prefixes, without a word
    /// boundary, and a one-letter last word is no alias.
    #[test]
    fn recalld_prefix_quirks_are_kept() {
        assert_eq!(canonicalize_entity("Drake"), vec!["ake"]);
        assert_eq!(canonicalize_entity("Mrs Smith"), vec!["s smith", "smith"]);
        assert_eq!(canonicalize_entity("John Q"), vec!["john q"]);
        assert_eq!(canonicalize_entity("Dr"), vec!["dr"]);
    }

    #[test]
    fn overlap_is_jaccard_of_first_keys() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert_eq!(entity_overlap(&[], &s(&["a"])), 0.0);
        assert_eq!(entity_overlap(&s(&["Alice", "Bob"]), &s(&["alice"])), 0.5);
        assert_eq!(
            entity_overlap(&s(&["Dr. John Smith"]), &s(&["john smith"])),
            1.0
        );
        assert_eq!(entity_overlap(&s(&["  "]), &s(&[" "])), 0.0);
    }

    // ── entity_index.rs ─────────────────────────────────────────────

    fn ns() -> NamespaceId {
        NamespaceId::new(1)
    }

    fn mid(n: u8) -> MemoryId {
        let mut bytes = [0u8; 16];
        bytes[15] = n;
        MemoryId::from_bytes(bytes)
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn add_and_find() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice", "Bob"]));
        idx.add(ns(), mid(2), &v(&["Alice", "Charlie"]));
        let results = idx.find_by_entities(ns(), &v(&["Alice"]), mid(1));
        assert_eq!(results, vec![(mid(2), 1)]);
    }

    #[test]
    fn case_insensitive() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice"]));
        idx.add(ns(), mid(2), &v(&["alice"]));
        let results = idx.find_by_entities(ns(), &v(&["ALICE"]), mid(1));
        assert_eq!(results, vec![(mid(2), 1)]);
    }

    #[test]
    fn remove_memory() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice"]));
        idx.add(ns(), mid(2), &v(&["Alice"]));
        idx.remove(ns(), mid(1));
        let results = idx.find_by_entities(ns(), &v(&["Alice"]), mid(3));
        assert_eq!(results, vec![(mid(2), 1)]);
    }

    #[test]
    fn remove_cleans_empty_postings() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice"]));
        idx.remove(ns(), mid(1));
        assert!(idx.index.is_empty());
    }

    #[test]
    fn exclude_id() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice"]));
        assert!(
            idx.find_by_entities(ns(), &v(&["Alice"]), mid(1))
                .is_empty()
        );
    }

    #[test]
    fn sorted_by_shared_count_desc() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &v(&["Alice", "Bob", "Charlie"]));
        idx.add(ns(), mid(2), &v(&["Alice"]));
        idx.add(ns(), mid(3), &v(&["Alice", "Bob"]));
        let results = idx.find_by_entities(ns(), &v(&["Alice", "Bob", "Charlie"]), mid(1));
        assert_eq!(results[0].0, mid(3));
        assert_eq!(results[1].0, mid(2));
    }

    #[test]
    fn empty_entities() {
        let mut idx = EntityIndex::new();
        idx.add(ns(), mid(1), &[]);
        assert_eq!(idx.len(), 0);
        assert!(idx.find_by_entities(ns(), &[], mid(1)).is_empty());
    }

    #[test]
    fn replacement_and_removal_are_idempotent_and_clear_all_aliases() {
        let mut idx = EntityIndex::new();
        let labels = v(&["Dr. John Smith", "JOHN SMITH", "José"]);
        for _ in 0..3 {
            idx.add(ns(), mid(1), &labels);
        }
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx.find_by_entities(ns(), &labels, mid(0)),
            vec![(mid(1), 3)]
        );
        idx.add(ns(), mid(1), &v(&["Alice's"]));
        assert_eq!(idx.len(), 1);
        assert!(idx.find_by_entities(ns(), &labels, mid(0)).is_empty());
        assert_eq!(
            idx.find_by_entities(ns(), &v(&["Alice", "ALICE"]), mid(0)),
            vec![(mid(1), 1)]
        );
        idx.add(ns(), mid(1), &[]);
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
        assert!(!idx.remove(ns(), mid(1)));
        idx.add(ns(), mid(1), &labels);
        idx.add(ns(), mid(1), &v(&["   "]));
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
    }

    #[test]
    fn namespaces_are_isolated_for_same_id_updates_removals_and_ties() {
        let mut idx = EntityIndex::new();
        let other = NamespaceId::new(2);
        let names = v(&["Alice"]);
        for id in [mid(3), mid(1), mid(2)] {
            idx.add(ns(), id, &names);
        }
        idx.add(other, mid(1), &v(&["Alice", "Bob"]));
        assert_eq!(idx.len(), 4);
        assert_eq!(
            idx.find_by_entities(ns(), &names, mid(0)),
            vec![(mid(1), 1), (mid(2), 1), (mid(3), 1)]
        );
        idx.add(ns(), mid(1), &v(&["Carol"]));
        assert_eq!(
            idx.find_by_entities(other, &names, mid(0)),
            vec![(mid(1), 1)]
        );
        assert!(idx.remove(ns(), mid(1)));
        assert!(!idx.remove(ns(), mid(1)));
        assert_eq!(idx.len(), 3);
        idx.remove_namespace(ns());
        idx.remove_namespace(ns());
        assert_eq!(idx.len(), 1);
        assert_eq!(
            idx.find_by_entities(other, &names, mid(0)),
            vec![(mid(1), 1)]
        );
        assert!(idx.find_by_entities(ns(), &names, mid(0)).is_empty());
        idx.remove_namespace(other);
        assert!(idx.is_empty());
        assert!(idx.index.is_empty());
    }

    /// From recalld `entity_rebuild_uses_canonical_labels_and_replaces_the_complete_view`:
    /// duplicate and alias labels collapse, Unicode names index, namespaces stay apart.
    #[test]
    fn canonical_labels_collapse_and_unicode_names_index() {
        let mut idx = EntityIndex::new();
        let other = NamespaceId::new(2);
        idx.add(
            ns(),
            mid(1),
            &v(&["Dr. John Smith", "JOHN SMITH", "José", "José"]),
        );
        idx.add(other, mid(2), &v(&["José"]));
        idx.add(ns(), mid(3), &[]);
        idx.add(ns(), mid(4), &v(&["日本語"]));
        assert_eq!(idx.len(), 3);
        let query = v(&["John Smith", "José", "José"]);
        assert_eq!(
            idx.find_by_entities(ns(), &query, mid(0)),
            vec![(mid(1), 3)]
        );
        assert_eq!(
            idx.find_by_entities(other, &query, mid(0)),
            vec![(mid(2), 1)]
        );
        assert_eq!(
            idx.find_by_entities(ns(), &v(&["日本語"]), mid(0)),
            vec![(mid(4), 1)]
        );
        assert_eq!(
            idx.find_by_entities_filtered(ns(), &query, mid(0), |id| id != mid(1)),
            vec![]
        );
    }
}
