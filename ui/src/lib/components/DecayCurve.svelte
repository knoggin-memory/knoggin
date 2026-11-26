<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- Forgetting curve for one memory (DESIGN §5 "Decay curve"), from now to
     +1 yr with recalld's FSRS formula. `previousStability` draws the curve
     before the most recent reinforce, dashed. -->
<script lang="ts">
  import { retrievability, THRESHOLDS } from '$lib/fsrs';

  let {
    stability,
    elapsedDays = 0,
    previousStability = null,
    thresholds = THRESHOLDS,
    multiplier,
    height = 96,
  }: {
    stability: number;
    elapsedDays?: number;
    previousStability?: number | null;
    thresholds?: [number, number, number];
    multiplier: number;
    height?: number;
  } = $props();

  // Geometry from reference/Explore: plot x 24→298 spans now→+1 yr, y 6→80 spans R 1→0.
  const W = 304, X0 = 24, X1 = 298, Y0 = 6;
  const Y1 = $derived(height - 16);
  const y = (r: number) => Y0 + (1 - r) * (Y1 - Y0);
  const x = (days: number) => X0 + (days / 365) * (X1 - X0);
  const curve = (s: number) =>
    Array.from({ length: 92 }, (_, i) => {
      const d = (i / 91) * 365;
      return `${x(d).toFixed(1)},${y(retrievability(elapsedDays + d, s, multiplier)).toFixed(1)}`;
    }).join(' ');
  const now = $derived(retrievability(elapsedDays, stability, multiplier));
</script>

<svg viewBox="0 0 {W} {height}" width="100%" {height} role="img" aria-label="Forgetting curve for the selected memory">
  <rect class="band-full" x={X0} y={Y0} width={X1 - X0} height={y(thresholds[0]) - Y0} />
  <rect class="band-summary" x={X0} y={y(thresholds[0])} width={X1 - X0} height={y(thresholds[1]) - y(thresholds[0])} />
  {#each [thresholds[0], thresholds[1]] as t (t)}
    <line class="grid" x1={X0} x2={X1} y1={y(t)} y2={y(t)} />
    <text class="label" x={X0 - 4} y={y(t) + 2.8} text-anchor="end">{t.toFixed(2)}</text>
  {/each}
  {#if previousStability !== null}
    <polyline class="previous" points={curve(previousStability)} />
  {/if}
  <polyline class="curve" points={curve(stability)} />
  <line class="now" x1={X0} x2={X0} y1={Y0} y2={Y1} />
  <circle class="dot" cx={X0} cy={y(now)} r="3.2" />
  <text class="label" x={X0} y={height - 4} text-anchor="middle">now</text>
  <text class="label" x={x(182.5)} y={height - 4} text-anchor="middle">+6 mo</text>
  <text class="label" x={X1} y={height - 4} text-anchor="end">+1 yr</text>
</svg>

<style>
  svg {
    display: block;
    overflow: visible;
  }
  .band-full {
    fill: var(--band-full);
  }
  .band-summary {
    fill: var(--band-summary);
  }
  .grid {
    stroke: var(--color-grid);
    stroke-dasharray: 2 3;
  }
  .label {
    font: var(--text-chart) var(--font-mono);
    fill: var(--color-text-axis);
  }
  .curve {
    fill: none;
    stroke: var(--color-accent);
    stroke-width: 2;
  }
  .previous {
    fill: none;
    stroke: var(--color-edge);
    stroke-width: 1.5;
    stroke-dasharray: 3 3;
  }
  .now {
    stroke: var(--color-accent);
    stroke-opacity: 0.45;
  }
  .dot {
    fill: var(--color-accent);
  }
</style>
