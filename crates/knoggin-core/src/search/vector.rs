// SPDX-License-Identifier: AGPL-3.0-only
//! The vector table: every vector of a store in one contiguous `Vec<f32>`, scanned exactly.

use std::collections::BTreeMap;

use crate::id::MemoryId;
use crate::model::limits::{VECTOR_NORM_MAX, VECTOR_NORM_MIN};

/// Dot product in a fixed order, so that x86_64 and arm64 give the same bits: eight
/// separate sums over chunks of eight, added in index order, then the tail in order.
/// No `mul_add`, no intrinsics.
pub fn dot(x: &[f32], y: &[f32]) -> f32 {
    let mut sums = [0.0_f32; 8];
    let (x_chunks, x_tail) = x.as_chunks::<8>();
    let (y_chunks, y_tail) = y.as_chunks::<8>();
    for (a, b) in x_chunks.iter().zip(y_chunks) {
        for ((sum, a), b) in sums.iter_mut().zip(a).zip(b) {
            *sum += a * b;
        }
    }
    let mut total = 0.0_f32;
    for s in sums {
        total += s;
    }
    for (a, b) in x_tail.iter().zip(y_tail) {
        total += a * b;
    }
    total
}

/// A vector the store accepts: the store's dimension, finite, norm 0.999 to 1.001.
pub fn is_valid_vector(vector: &[f32], dimensions: usize) -> bool {
    if vector.len() != dimensions || !vector.iter().all(|v| v.is_finite()) {
        return false;
    }
    let norm = dot(vector, vector).sqrt();
    (VECTOR_NORM_MIN..=VECTOR_NORM_MAX).contains(&norm)
}

/// `count * dimension` values, a table from slot to ID and one from ID to slot.
/// Removing a vector moves the last slot into the hole.
#[derive(Clone, Default, PartialEq)]
pub struct VectorTable {
    dimensions: usize,
    data: Vec<f32>,
    ids: Vec<MemoryId>,
    slots: BTreeMap<MemoryId, usize>,
}

impl std::fmt::Debug for VectorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorTable")
            .field("dimensions", &self.dimensions)
            .field("len", &self.ids.len())
            .finish()
    }
}

impl VectorTable {
    /// An empty table for vectors of `dimensions` values.
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions,
            ..Self::default()
        }
    }

    /// Values per vector.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Number of vectors.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// No vectors.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether `id` has a vector.
    pub fn contains(&self, id: MemoryId) -> bool {
        self.slots.contains_key(&id)
    }

    /// The vector of `id`.
    pub fn get(&self, id: MemoryId) -> Option<&[f32]> {
        let slot = *self.slots.get(&id)?;
        let start = slot.checked_mul(self.dimensions)?;
        self.data.get(start..start.checked_add(self.dimensions)?)
    }

    /// Stores or replaces the vector of `id`. Refuses (returns false) a vector of the
    /// wrong length; the caller has already checked it with [`is_valid_vector`].
    pub fn insert(&mut self, id: MemoryId, vector: &[f32]) -> bool {
        if vector.len() != self.dimensions {
            return false;
        }
        if let Some(&slot) = self.slots.get(&id) {
            let start = slot * self.dimensions;
            return match self.data.get_mut(start..start + self.dimensions) {
                Some(dst) => {
                    dst.copy_from_slice(vector);
                    true
                }
                None => false,
            };
        }
        self.slots.insert(id, self.ids.len());
        self.ids.push(id);
        self.data.extend_from_slice(vector);
        true
    }

    /// Removes the vector of `id`, moving the last slot into its place.
    pub fn remove(&mut self, id: MemoryId) -> Option<Vec<f32>> {
        let slot = self.slots.remove(&id)?;
        let dim = self.dimensions;
        let last = self.ids.len().checked_sub(1)?;
        let removed = self.data.get(slot * dim..(slot + 1) * dim)?.to_vec();
        if slot != last {
            let moved = *self.ids.get(last)?;
            self.data
                .copy_within(last * dim..(last + 1) * dim, slot * dim);
            *self.ids.get_mut(slot)? = moved;
            self.slots.insert(moved, slot);
        }
        self.ids.pop();
        self.data.truncate(last * dim);
        Some(removed)
    }

    /// Drops every vector and takes a new dimension (a model change).
    pub fn reset(&mut self, dimensions: usize) {
        *self = Self::new(dimensions);
    }

    /// The table from ID to slot that lookups read. For index comparisons.
    pub fn slots(&self) -> &BTreeMap<MemoryId, usize> {
        &self.slots
    }

    /// Every vector in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (MemoryId, &[f32])> {
        self.ids
            .iter()
            .copied()
            .zip(self.data.chunks_exact(self.dimensions.max(1)))
    }

    /// Exact top `k` by dot product with `query` over the vectors for which `eligible`
    /// holds: highest first, ties to the smaller ID.
    pub fn top_k(
        &self,
        query: &[f32],
        k: usize,
        mut eligible: impl FnMut(MemoryId) -> bool,
    ) -> Vec<(MemoryId, f32)> {
        if k == 0 || query.len() != self.dimensions {
            return Vec::new();
        }
        let mut scored: Vec<(MemoryId, f32)> = self
            .iter()
            .filter(|(id, _)| eligible(*id))
            .map(|(id, v)| (id, dot(query, v)))
            .collect();
        let by_rank =
            |a: &(MemoryId, f32), b: &(MemoryId, f32)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        if scored.len() > k {
            scored.select_nth_unstable_by(k - 1, by_rank);
            scored.truncate(k);
        }
        scored.sort_by(by_rank);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> MemoryId {
        let mut b = [0; 16];
        b[15] = n;
        MemoryId::from_bytes(b)
    }

    #[test]
    fn dot_is_fixed_order_and_handles_the_tail() {
        let x: Vec<f32> = (0..19).map(|i| i as f32 * 0.5).collect();
        let y: Vec<f32> = (0..19).map(|i| 1.0 - i as f32 * 0.25).collect();
        let mut lanes = [0.0_f32; 8];
        for i in 0..16 {
            lanes[i % 8] += x[i] * y[i];
        }
        let mut expected = 0.0_f32;
        for l in lanes {
            expected += l;
        }
        for i in 16..19 {
            expected += x[i] * y[i];
        }
        assert_eq!(dot(&x, &y).to_bits(), expected.to_bits());
        assert_eq!(dot(&[], &[]), 0.0);
        assert_eq!(dot(&[2.0], &[3.0]), 6.0);
    }

    #[test]
    fn vectors_are_validated() {
        let mut v = vec![0.0_f32; 4];
        v[0] = 1.0;
        assert!(is_valid_vector(&v, 4));
        assert!(!is_valid_vector(&v, 5));
        v[1] = f32::NAN;
        assert!(!is_valid_vector(&v, 4));
        assert!(!is_valid_vector(&[0.5, 0.5, 0.0, 0.0], 4));
    }

    #[test]
    fn swap_remove_keeps_slots_consistent() {
        let mut t = VectorTable::new(2);
        for n in 1..=4 {
            assert!(t.insert(id(n), &[f32::from(n), 0.0]));
        }
        assert!(!t.insert(id(9), &[1.0]));
        assert_eq!(t.remove(id(2)), Some(vec![2.0, 0.0]));
        assert_eq!(t.len(), 3);
        assert_eq!(t.get(id(4)), Some(&[4.0, 0.0][..]));
        assert_eq!(t.get(id(2)), None);
        let order: Vec<_> = t.iter().map(|(i, _)| i).collect();
        assert_eq!(order, [id(1), id(4), id(3)]);
        assert_eq!(t.remove(id(3)), Some(vec![3.0, 0.0]));
        assert_eq!(t.remove(id(3)), None);
        assert!(t.insert(id(1), &[9.0, 9.0]));
        assert_eq!(t.get(id(1)), Some(&[9.0, 9.0][..]));
        assert_eq!(t.len(), 2);
        t.reset(3);
        assert!(t.is_empty());
        assert_eq!(t.dimensions(), 3);
    }

    #[test]
    fn top_k_breaks_ties_by_smaller_id_and_filters() {
        let mut t = VectorTable::new(2);
        t.insert(id(3), &[1.0, 0.0]);
        t.insert(id(1), &[1.0, 0.0]);
        t.insert(id(2), &[0.0, 1.0]);
        t.insert(id(4), &[0.6, 0.8]);
        let top = t.top_k(&[1.0, 0.0], 3, |_| true);
        assert_eq!(top, [(id(1), 1.0), (id(3), 1.0), (id(4), 0.6)]);
        let top = t.top_k(&[1.0, 0.0], 10, |i| i != id(1));
        assert_eq!(
            top.iter().map(|x| x.0).collect::<Vec<_>>(),
            [id(3), id(4), id(2)]
        );
        assert!(t.top_k(&[1.0, 0.0], 0, |_| true).is_empty());
        assert!(t.top_k(&[1.0], 3, |_| true).is_empty());
    }
}
