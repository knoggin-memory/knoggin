// SPDX-License-Identifier: AGPL-3.0-only
//! Test helpers: the fake embedding provider. `StoreBuilder` goes here too.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use crate::embed::{EmbedError, EmbeddingProvider, ModelIdentity};
use crate::search::vector::dot;

/// A deterministic stand-in for the model: texts that share words get a higher cosine.
///
/// Each white-space token, with its ASCII letters lower-cased, seeds a 64-bit FNV-1a
/// hash; a linear congruential step then yields one value per dimension. The token
/// vectors are added and the sum is divided by its norm. A text with no token gives
/// the unit vector along the first axis. Only integer arithmetic and `f32` addition,
/// multiplication, division and `sqrt` are used, so every platform gives the same bits.
#[derive(Debug, Clone)]
pub struct FakeProvider {
    identity: ModelIdentity,
}

impl Default for FakeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeProvider {
    /// Identity `fake-64`, 64 dimensions.
    pub fn new() -> Self {
        Self::with_dim(64)
    }

    /// Identity `fake-<dim>`; the budget test uses 768 so that a scan costs what it costs
    /// in production.
    pub fn with_dim(dim: u32) -> Self {
        Self {
            identity: ModelIdentity {
                id: format!("fake-{dim}"),
                dimensions: dim,
            },
        }
    }

    /// The vector of `text`, without the `Result`.
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let dim = self.identity.dimensions as usize;
        let mut sum = vec![0.0_f32; dim];
        let mut any = false;
        for token in text.split_whitespace() {
            any = true;
            let mut x = fnv1a(token.to_ascii_lowercase().as_bytes());
            for value in &mut sum {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                *value += ((x >> 40) as f32) / 8_388_608.0 - 1.0;
            }
        }
        let norm = dot(&sum, &sum).sqrt();
        if !any || norm == 0.0 || !norm.is_finite() {
            let mut unit = vec![0.0_f32; dim];
            if let Some(first) = unit.first_mut() {
                *first = 1.0;
            }
            return unit;
        }
        for value in &mut sum {
            *value /= norm;
        }
        sum
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl EmbeddingProvider for FakeProvider {
    fn identity(&self) -> &ModelIdentity {
        &self.identity
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.vector(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::vector::is_valid_vector;

    #[test]
    fn identities() {
        assert_eq!(FakeProvider::new().identity().id, "fake-64");
        assert_eq!(FakeProvider::new().identity().dimensions, 64);
        let big = FakeProvider::with_dim(768);
        assert_eq!(big.identity().id, "fake-768");
        assert_eq!(big.vector("hello world").len(), 768);
    }

    #[test]
    fn vectors_are_unit_deterministic_and_case_folded() {
        let p = FakeProvider::new();
        let a = p.embed("Maya prefers window seats").unwrap();
        assert!(is_valid_vector(&a, 64));
        assert_eq!(a, p.embed("maya PREFERS window seats").unwrap());
        assert_eq!(a, p.embed("  maya prefers\twindow seats ").unwrap());
        let empty = p.embed("   ").unwrap();
        assert_eq!(empty[0], 1.0);
        assert!(empty[1..].iter().all(|v| *v == 0.0));
        // Only ASCII letters are lower-cased.
        assert_ne!(p.embed("É").unwrap(), p.embed("é").unwrap());
    }

    #[test]
    fn shared_words_score_higher() {
        let p = FakeProvider::new();
        let q = p.vector("window seats on the plane");
        let near = p.vector("Maya prefers window seats");
        let far = p.vector("quarterly revenue grew strongly");
        assert!(dot(&q, &near) > dot(&q, &far));
        assert!((dot(&near, &near) - 1.0).abs() < 1e-5);
    }

    /// Pins the algorithm: a change to the hash or the step changes this value.
    #[test]
    fn first_value_is_pinned() {
        let v = FakeProvider::new().vector("a");
        let mut x = fnv1a(b"a");
        assert_eq!(x, 0xaf63_dc4c_8601_ec8c);
        x = x
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let raw0 = ((x >> 40) as f32) / 8_388_608.0 - 1.0;
        let all = {
            let mut y = fnv1a(b"a");
            (0..64)
                .map(|_| {
                    y = y
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    ((y >> 40) as f32) / 8_388_608.0 - 1.0
                })
                .collect::<Vec<f32>>()
        };
        let norm = dot(&all, &all).sqrt();
        assert_eq!(v[0], raw0 / norm);
    }
}
