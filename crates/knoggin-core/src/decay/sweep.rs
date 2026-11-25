// SPDX-License-Identifier: AGPL-3.0-only
//! Phase planning and the sweep.
//!
//! The planning rule is ported from recalld `src/storage/native_maintenance.rs`
//! (`plan`, lines 95 to 166): `Full` fades when effective strength is at or below
//! `full_to_summary`, `Summary` when it is at or below `summary_to_ghost`, and a ghost
//! is deleted when raw strength is at or below `ghost_to_delete`. Permanent memories
//! and namespaces whose multiplier is 0 are skipped. Every change is decided before
//! any is applied, so a memory moves at most one step per sweep.

use crate::id::{MemoryId, NamespaceId};
use crate::model::{Memory, NamespacePolicy, Phase, PhaseThresholds};
use crate::ops::{SweepReport, WouldChange};
use crate::store::Store;
use crate::time::Timestamp;

/// One planned step down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseStep {
    /// The full text fades.
    ToSummary,
    /// Summary, labels and vector go.
    ToGhost,
    /// The ghost is removed.
    Delete,
}

/// The next step of a memory in `phase` with raw strength `raw` and effective strength
/// `effective`, or `None` when it stays.
pub fn plan_phase(
    phase: Phase,
    raw: f32,
    effective: f32,
    thresholds: &PhaseThresholds,
) -> Option<PhaseStep> {
    match phase {
        Phase::Full if effective <= thresholds.full_to_summary => Some(PhaseStep::ToSummary),
        Phase::Summary if effective <= thresholds.summary_to_ghost => Some(PhaseStep::ToGhost),
        Phase::Ghost if raw <= thresholds.ghost_to_delete => Some(PhaseStep::Delete),
        _ => None,
    }
}

impl Store {
    /// The step of one memory at `now` under its namespace's current policy.
    fn plan_memory(&self, memory: &Memory, now: Timestamp) -> Option<PhaseStep> {
        let policy = self.namespaces.get(&memory.namespace)?.policy;
        if memory.is_permastore || policy.multiplier() == 0.0 {
            return None;
        }
        let (raw, effective) = self.strengths(memory, now);
        plan_phase(memory.phase, raw, effective, &policy.phase_thresholds)
    }

    /// Every planned step, in ID order, of the memories `only` selects.
    fn plan_sweep(&self, only: Option<NamespaceId>, now: Timestamp) -> Vec<(MemoryId, PhaseStep)> {
        self.memories
            .values()
            .filter(|m| only.is_none_or(|ns| m.namespace == ns))
            .filter_map(|m| Some((m.id, self.plan_memory(m, now)?)))
            .collect()
    }

    /// Moves every memory that has faded one phase down, and deletes ghosts that have
    /// faded out. Runs when a store is opened and every 24 hours while it is open.
    pub fn sweep(&mut self, now: Timestamp) -> SweepReport {
        let mut report = SweepReport::default();
        for (id, step) in self.plan_sweep(None, now) {
            let done = match step {
                PhaseStep::ToSummary => self.fade_to_summary(id, now),
                PhaseStep::ToGhost => self.fade_to_ghost(id, now),
                PhaseStep::Delete => self.remove_memory(id).is_some(),
            };
            if !done {
                continue;
            }
            match step {
                PhaseStep::ToSummary => report.full_to_summary += 1,
                PhaseStep::ToGhost => report.summary_to_ghost += 1,
                PhaseStep::Delete => report.deleted += 1,
            }
            if step == PhaseStep::Delete {
                report.removed.push(id);
            } else {
                report.phase_changed.push(id);
            }
        }
        if !report.phase_changed.is_empty() || !report.removed.is_empty() {
            self.changes.bookkeeping = self.changes.bookkeeping.saturating_add(1);
        }
        report
    }

    /// What the next sweep would do to `namespace` under `policy`, changing nothing.
    pub(crate) fn would_change(
        &mut self,
        namespace: NamespaceId,
        policy: NamespacePolicy,
        now: Timestamp,
    ) -> WouldChange {
        // Strengths read the policy from the namespace, so the new one goes in for the
        // planning and the old one comes back; no index depends on it.
        let Some(old) = self
            .namespaces
            .get_mut(&namespace)
            .map(|n| std::mem::replace(&mut n.policy, policy))
        else {
            return WouldChange {
                full_to_summary: 0,
                summary_to_ghost: 0,
                deleted: 0,
            };
        };
        let plan = self.plan_sweep(Some(namespace), now);
        if let Some(n) = self.namespaces.get_mut(&namespace) {
            n.policy = old;
        }
        let count = |step| plan.iter().filter(|(_, s)| *s == step).count() as u64;
        WouldChange {
            full_to_summary: count(PhaseStep::ToSummary),
            summary_to_ghost: count(PhaseStep::ToGhost),
            deleted: count(PhaseStep::Delete),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::{Edge, EdgeType};
    use crate::store::IdempotencyEntry;
    use crate::store::write::tests::{
        DAY, T0, assert_matches_rebuild, ctx, mid, req, save, store, t,
    };

    #[test]
    fn plan_phase_uses_effective_strength_for_content_and_raw_for_ghosts() {
        let th = PhaseThresholds::default();
        assert_eq!(plan_phase(Phase::Full, 0.1, 0.71, &th), None);
        assert_eq!(
            plan_phase(Phase::Full, 0.1, 0.7, &th),
            Some(PhaseStep::ToSummary)
        );
        assert_eq!(
            plan_phase(Phase::Full, 0.0, 0.0, &th),
            Some(PhaseStep::ToSummary),
            "one step only"
        );
        assert_eq!(plan_phase(Phase::Summary, 0.1, 0.31, &th), None);
        assert_eq!(
            plan_phase(Phase::Summary, 0.1, 0.3, &th),
            Some(PhaseStep::ToGhost)
        );
        assert_eq!(plan_phase(Phase::Ghost, 0.06, 0.0, &th), None);
        assert_eq!(
            plan_phase(Phase::Ghost, 0.05, 1.0, &th),
            Some(PhaseStep::Delete)
        );
    }

    /// Days until raw strength with the default stability falls to 0.7, 0.3, 0.05:
    /// about 16.5, 160 and 6,318.
    const TO_SUMMARY: i64 = 17 * DAY;
    const TO_GHOST: i64 = 161 * DAY;
    const TO_DELETE: i64 = 6_320 * DAY;

    #[test]
    fn a_memory_moves_one_phase_per_sweep() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        assert_eq!(
            s.sweep(t(T0 + TO_SUMMARY - 2 * DAY)),
            SweepReport::default()
        );
        assert_eq!(s.changes().bookkeeping, 0);
        let late = t(T0 + TO_DELETE);
        let r = s.sweep(late);
        assert_eq!(
            (r.full_to_summary, r.phase_changed.as_slice()),
            (1, &[mid(1)][..])
        );
        let m = s.memory(mid(1)).unwrap();
        assert_eq!(
            (m.phase, m.full_text.is_none(), m.phase_changed_at),
            (Phase::Summary, true, late)
        );
        assert_eq!(s.changes().bookkeeping, 1);
        assert_eq!(s.sweep(late).summary_to_ghost, 1);
        assert_eq!(s.memory(mid(1)).unwrap().phase, Phase::Ghost);
        let r = s.sweep(late);
        assert_eq!((r.deleted, r.removed.as_slice()), (1, &[mid(1)][..]));
        assert!(s.memory(mid(1)).is_none());
        assert_eq!(s.changes().bookkeeping, 3);
        assert_matches_rebuild(&mut s);
    }

    #[test]
    fn thresholds_are_reached_at_the_documented_ages() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        assert_eq!(s.sweep(t(T0 + TO_SUMMARY)).full_to_summary, 1);
        assert_eq!(s.sweep(t(T0 + TO_GHOST - 2 * DAY)).summary_to_ghost, 0);
        assert_eq!(s.sweep(t(T0 + TO_GHOST)).summary_to_ghost, 1);
        assert_eq!(s.sweep(t(T0 + TO_DELETE - 10 * DAY)).deleted, 0);
        assert_eq!(s.sweep(t(T0 + TO_DELETE)).deleted, 1);
    }

    #[test]
    fn permastore_and_a_zero_multiplier_are_skipped() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        save(&mut s, 2, "beta");
        s.update_memory(mid(1), |m| m.is_permastore = true);
        crate::store::write::tests::namespace(&mut s, "frozen");
        let up = crate::ops::UpdatePolicy {
            name: "frozen".into(),
            policy: NamespacePolicy {
                decay_rate_multiplier: Some(0.0),
                ..NamespacePolicy::default()
            },
            dry_run: false,
        };
        s.update_namespace_policy(up, &ctx(), t(T0)).unwrap();
        s.store_memory(
            req(json!({ "summary": "c", "namespace": "frozen" })),
            mid(3),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        let r = s.sweep(t(T0 + TO_DELETE));
        assert_eq!(r.phase_changed, [mid(2)]);
    }

    #[test]
    fn the_connection_bonus_holds_a_memory_back() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        save(&mut s, 2, "beta");
        s.update_memory(mid(2), |m| m.stability = 36_500.0);
        // Both were saved at the same time, so they share a temporal link of weight 1.
        assert_eq!(s.graph().len(), 1);
        let at = t(T0 + TO_SUMMARY);
        let (raw, eff) = s.strengths(s.memory(mid(1)).unwrap(), at);
        assert!(raw <= 0.7 && eff > 0.7, "{raw} {eff}");
        assert_eq!(s.sweep(at).full_to_summary, 0);
    }

    #[test]
    fn ghosting_purges_idempotency_entries_and_keeps_edges() {
        let mut s = store();
        save(&mut s, 1, "alpha");
        s.store_memory(
            req(json!({ "summary": "b", "createdAt": "2020-01-01T00:00:00Z" })),
            mid(2),
            None,
            &ctx(),
            t(T0),
        )
        .unwrap();
        s.insert_edge(Edge {
            source: mid(1),
            target: mid(2),
            edge_type: EdgeType::ParentChild,
            weight: 1.0,
            auto_created: false,
            created_at: t(T0),
        });
        s.insert_idempotency(
            "k".into(),
            IdempotencyEntry {
                request_hash: [0; 32],
                created_at: t(T0),
                response: b"{}".to_vec(),
                text_ids: vec![mid(1)],
            },
        );
        s.update_memory(mid(1), |m| m.phase = Phase::Summary);
        let r = s.sweep(t(T0 + 200 * DAY));
        assert!(r.phase_changed.contains(&mid(1)));
        assert_eq!(s.memory(mid(1)).unwrap().phase, Phase::Ghost);
        assert!(s.idempotency.is_empty());
        assert_eq!(s.graph().len(), 1);
        assert_matches_rebuild(&mut s);
    }
}
