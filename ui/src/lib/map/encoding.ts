// SPDX-License-Identifier: AGPL-3.0-only
// The map's visual encoding, as graphology attributes for sigma.
import type { MapTheme } from './theme';
import { alpha, mix } from './theme';
import { displaySize } from './protocol';

// The graph route keeps recalld's numbers for edge types and phases;
// every other answer and every event names them.
export const EDGE = { parent: 1, associative: 2, entity: 5, temporal: 6, supersedes: 7 } as const;
export const EDGE_CODE: Record<string, number> = { parent_child: 1, associative: 2, entity: 5, temporal: 6, supersedes: 7 };
export const EDGE_LABELS: Record<number, string> = {
  1: 'Parent and child', 2: 'Associative', 5: 'Entity', 6: 'Temporal', 7: 'Supersedes',
};
export const EDGE_NAME_LABELS: Record<string, string> = {
  parent_child: 'Parent and child', associative: 'Associative', entity: 'Entity', temporal: 'Temporal', supersedes: 'Supersedes',
};
export const PHASE_CODE: Record<string, number> = { full: 1, summary: 2, ghost: 3 };

/** A memory's look by phase: each phase has its own hue,
 *  all solid. Full runs node → node-fading, Summary node-summary →
 *  node-summary-fading, Ghost is a solid node-ghost with no ring. Lighter
 *  means more faded. `strength` is retrievability now (0..1). */
export function nodeAttributes(theme: MapTheme, phase: number, degree: number, permastore: boolean, strength = 1) {
  const size = displaySize(degree, strength);
  const r = Math.min(1, Math.max(0, strength));
  switch (phase) {
    case 1: {
      // Full: R 1 → 0.7 runs Forest → moss.
      const t = Math.min(1, Math.max(0, (1 - r) / 0.3));
      return { size, color: mix(theme.node, theme.nodeFading, t), ring: alpha(theme.node, 0), visibility: 'visible' as const, permastore };
    }
    case 2: {
      // Summary: R 0.7 → 0.3 runs Terracotta → a pale orange.
      const t = Math.min(1, Math.max(0, (0.7 - r) / 0.4));
      return { size, color: mix(theme.nodeSummary, theme.nodeSummaryFading, t), ring: alpha(theme.node, 0), visibility: 'visible' as const, permastore };
    }
    case 3:
      // Ghost: a solid slate blue, no outline.
      return { size, color: theme.nodeGhost, ring: alpha(theme.nodeGhost, 0), visibility: 'visible' as const, permastore };
    default:
      // Tombstones and memories the decay projection deletes.
      return { size: 1.6, color: alpha(theme.nodeGhost, 0.35), ring: alpha(theme.node, 0), visibility: 'hidden' as const, permastore: false };
  }
}

export function edgeAttributes(theme: MapTheme, type: number, weight: number) {
  // Soft solid lines whose opacity carries the weight, so weak links recede
  // and dense clusters read as a haze rather than a scratchy mesh. Only
  // Supersedes links are dashed, in the cool slate: fine dashes in a dense
  // cluster look like noise.
  const a = 0.32 * (0.3 + 0.7 * weight);
  const dashed = type === EDGE.supersedes ? [3, 3] : null;
  const base = dashed ? theme.supersedes : theme.edge;
  const color = alpha(base, Math.min(1, a * (dashed ? 1.25 : 1)));
  return {
    size: dashed ? 1.2 : 1,
    color: dashed ? alpha(base, 0) : color,
    dashColor: dashed ? color : alpha(base, 0),
    dashSize: dashed ? dashed[0] : 0,
    gapSize: dashed ? dashed[1] : 0,
    baseColor: color,
  };
}
