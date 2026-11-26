// SPDX-License-Identifier: AGPL-3.0-only
//! The FSRS v4.5 forgetting curve and stability update: pure functions.
//!
//! Ported from recalld `src/decay/fsrs.rs` (the calculator, without its config
//! struct) and the feedback rule of `src/decay/lifecycle.rs` (`plan_feedback_state`).

use crate::model::limits::{STABILITY_CEILING, STABILITY_FLOOR};

/// Power-law exponent of the curve `R(t, S) = (1 + FACTOR * t / S) ^ DECAY`.
const DECAY: f32 = -0.5;
/// Scaling factor, from the constraint `R(S, S) = 0.9`: `19 / 81`.
const FACTOR: f32 = 19.0 / 81.0;
/// Base growth rate of the stability increase (w8).
const W8: f32 = 1.6474;
/// Stability damping exponent (w9).
const W9: f32 = 0.1367;
/// Spacing effect strength (w10).
const W10: f32 = 1.0461;
/// Post-lapse stability factor (w11).
const W11: f32 = 1.2;
/// Post-lapse difficulty exponent (w12); 0 in FSRS v4.5.
const W12: f32 = 0.0;
/// Post-lapse stability exponent (w13).
const W13: f32 = 0.3261;
/// Post-lapse retrievability factor (w14); 0 in FSRS v4.5.
const W14: f32 = 0.0;

/// Raw strength: `R(t, S) = (1 + (19/81) * (t / m) / S) ^ (-0.5)`.
///
/// `elapsed_days` is days since the decay anchor (never negative; see
/// [`crate::time::elapsed_days`]), `stability` is S, `multiplier` is the namespace's
/// decay-rate multiplier. `R = 1` when `t = 0` or `m = 0`.
pub fn retrievability(elapsed_days: f32, stability: f32, multiplier: f32) -> f32 {
    if elapsed_days == 0.0 || multiplier == 0.0 {
        return 1.0;
    }
    let effective_elapsed = elapsed_days / multiplier;
    (1.0 + FACTOR * effective_elapsed / stability).powf(DECAY)
}

/// The stability increase factor `SInc = e^w8 * (11 - D) * S^(-w9) * (e^(w10 * (1 - R)) - 1) + 1`,
/// never below 1.
pub fn stability_increase(stability: f32, retrievability: f32, difficulty: f32) -> f32 {
    let growth_base = W8.exp();
    let difficulty_factor = 11.0 - difficulty;
    let stability_damping = stability.powf(-W9);
    let spacing_bonus = (W10 * (1.0 - retrievability)).exp() - 1.0;
    (growth_base * difficulty_factor * stability_damping * spacing_bonus + 1.0).max(1.0)
}

/// Effective strength `R_eff = R + bonus * (1 - R)`, kept in 0 to 1.
pub fn effective_retrievability(raw: f32, bonus: f32) -> f32 {
    (raw + bonus * (1.0 - raw)).clamp(0.0, 1.0)
}

/// The stability after a reinforcement of `quality` 1 to 4.
///
/// Quality 2 to 4: `S' = S * (1 + (SInc - 1) * g)` with `g` = 0.7, 1.0, 1.3.
/// Quality 1 (forgot): the post-lapse formula `S' = 1.2 * ((S + 1)^0.3261 - 1)`.
/// Either way clamped to 0.01 to 36,500 days. The caller has checked `quality`.
pub fn review_stability(
    stability: f32,
    elapsed_days: f32,
    quality: u8,
    multiplier: f32,
    difficulty: f32,
) -> f32 {
    let current_r = retrievability(elapsed_days, stability, multiplier);
    if quality == 1 {
        let lapse = W11
            * difficulty.powf(-W12)
            * ((stability + 1.0).powf(W13) - 1.0)
            * (W14 * (1.0 - current_r)).exp();
        return lapse.clamp(STABILITY_FLOOR, STABILITY_CEILING);
    }
    let base_sinc = stability_increase(stability, current_r, difficulty);
    let grade = match quality {
        2 => 0.7,
        3 => 1.0,
        _ => 1.3,
    };
    (stability * (1.0 + (base_sinc - 1.0) * grade)).clamp(STABILITY_FLOOR, STABILITY_CEILING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::limits::{DEFAULT_DIFFICULTY, DEFAULT_INITIAL_STABILITY};

    // Ported from recalld `fsrs::timeline_tests::default_stability_timeline_matches_the_docs`.
    #[test]
    fn default_stability_timeline_matches_the_docs() {
        // The documented days at which the default stability reaches each threshold.
        let r = |days| retrievability(days, DEFAULT_INITIAL_STABILITY, 1.0);
        assert!((r(3.7145) - 0.9).abs() < 1e-3);
        assert!((r(16.5) - 0.7).abs() < 1e-3);
        assert!((r(160.0) - 0.3).abs() < 1e-3);
        assert!((r(6318.0) - 0.05).abs() < 1e-3);
    }

    #[test]
    fn retrievability_is_one_at_zero_time_or_zero_multiplier_and_falls_with_time() {
        assert_eq!(retrievability(0.0, 3.7, 1.0), 1.0);
        assert_eq!(retrievability(100.0, 3.7, 0.0), 1.0);
        let at_s = retrievability(DEFAULT_INITIAL_STABILITY, DEFAULT_INITIAL_STABILITY, 1.0);
        assert!((at_s - 0.9).abs() < 1e-5, "{at_s}");
        assert!(retrievability(10.0, 3.7, 1.0) > retrievability(20.0, 3.7, 1.0));
        // A multiplier of 2 makes memories look half as old.
        assert_eq!(
            retrievability(10.0, 3.7, 2.0),
            retrievability(5.0, 3.7, 1.0)
        );
        assert!(retrievability(f32::MAX, 0.01, 1.0) >= 0.0);
    }

    #[test]
    fn effective_strength_adds_a_damped_bonus() {
        assert_eq!(effective_retrievability(0.5, 0.1), 0.55);
        assert_eq!(effective_retrievability(1.0, 0.15), 1.0);
        assert_eq!(effective_retrievability(0.0, 0.15), 0.15);
    }

    #[test]
    fn stability_increase_never_weakens() {
        assert!(stability_increase(3.7, 1.0, 5.0) >= 1.0);
        assert!(stability_increase(3.7, 0.3, 5.0) > stability_increase(3.7, 0.9, 5.0));
    }

    // From recalld `lifecycle::tests::policy_inheritance_multiplier_direction_and_record_difficulty`:
    // reviewing at 8 days, the multiplier orders the new stability.
    #[test]
    fn multiplier_direction_of_review() {
        let s = DEFAULT_INITIAL_STABILITY;
        let by = |m| review_stability(s, 8.0, 3, m, 8.0);
        assert_eq!(by(0.0), s, "no decay means R = 1 and SInc = 1");
        assert!(by(1.0) > by(2.0) && by(2.0) > by(0.0));
    }

    #[test]
    fn quality_orders_growth_and_quality_one_lapses() {
        let s = DEFAULT_INITIAL_STABILITY;
        let q = |quality| review_stability(s, 8.0, quality, 1.0, DEFAULT_DIFFICULTY);
        assert!(q(4) > q(3) && q(3) > q(2) && q(2) > s);
        let lapse = q(1);
        assert!(lapse < s);
        assert_eq!(
            lapse,
            (1.2 * ((s + 1.0).powf(0.3261) - 1.0)).clamp(STABILITY_FLOOR, STABILITY_CEILING)
        );
    }

    #[test]
    fn review_clamps_to_the_stability_bounds() {
        assert_eq!(
            review_stability(36_500.0, 100_000.0, 4, 1.0, 1.0),
            STABILITY_CEILING
        );
        assert_eq!(review_stability(0.01, 0.0, 1, 1.0, 5.0), STABILITY_FLOOR);
    }
}
