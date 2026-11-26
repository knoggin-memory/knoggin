<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<script lang="ts">
  import { createQuery, useQueryClient } from '@tanstack/svelte-query';
  import { scaleBand, scaleLinear } from 'd3-scale';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import { scope } from '$lib/scope.svelte';
  import { live } from '$lib/live.svelte';
  import { activity } from '$lib/api/memories';
  import type { ActivityRow } from '$lib/api/shapes';
  import { count as fmtCount, ms, truncateWords } from '$lib/format';
  import { storeKey } from '$lib/queries';

  const RANGES = [
    { key: '30', label: '30 days', days: 30 },
    { key: '90', label: '90 days', days: 90 },
    { key: '365', label: '1 year', days: 365 },
    { key: 'all', label: 'All time', days: null },
  ] as const;
  let range = $state<(typeof RANGES)[number]['key']>('90');
  const since = $derived.by(() => {
    const r = RANGES.find((x) => x.key === range)!;
    return r.days === null ? null : new Date(Date.now() - r.days * 86_400_000).toISOString();
  });

  // No timer: the rows come again when an event arrives (store routes are never polled).
  const data = createQuery(() => ({
    queryKey: storeKey('activity', scope.current, range),
    queryFn: () => activity(scope.current, since),
  }));
  const client = useQueryClient();
  $effect(() => {
    live.start();
    let t: ReturnType<typeof setTimeout> | undefined;
    const off = live.subscribe(({ event }) => {
      if (!['store', 'recall', 'reinforce', 'sweep'].includes(event.kind)) return;
      clearTimeout(t);
      t = setTimeout(() => client.invalidateQueries({ queryKey: storeKey('activity') }), 400);
    });
    return () => {
      off();
      clearTimeout(t);
      live.release();
    };
  });

  // ── Chart: stored per bucket stacked by current phase, memories recalled mirrored below ──
  const W = 1000, H = 240, AXIS = 132;
  const buckets = $derived((data.data?.buckets ?? []).map((b) => ({ ...b, t: ms(b.start) })));
  const maxUp = $derived(Math.max(1, ...buckets.map((b) => b.full + b.summary + b.ghost)));
  const maxDown = $derived(Math.max(1, ...buckets.map((b) => b.recalls)));
  const x = $derived(scaleBand<number>().domain(buckets.map((_, i) => i)).range([0, W]).paddingInner(0.25));
  const up = $derived(scaleLinear().domain([0, maxUp]).range([0, AXIS - 12]));
  const down = $derived(scaleLinear().domain([0, maxDown]).range([0, H - AXIS - 20]));
  const monthly = $derived((data.data?.bucketMs ?? 0) > 8 * 86_400_000);
  const fmt = (t: number) => new Date(t).toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
  const ticks = $derived(buckets.filter((_, i) => i % Math.max(1, Math.ceil(buckets.length / 8)) === 0));

  // ── Stream: the route's rows, which already hold the events the store keeps ──
  // A row names the agent only when Knoggin knows it; without one it starts with the verb.
  const VERB = { recall: 'recalled', store: 'stored', reinforce: 'reinforced' } as const;
  interface Line {
    at: number;
    kind: ActivityRow['kind'];
    lead: string;
    text: string;
    meta: string;
  }
  function line(r: ActivityRow): Line {
    const at = ms(r.at);
    if (r.kind === 'sweep') return { at, kind: r.kind, lead: 'Knoggin ran a decay sweep', text: '', meta: `${fmtCount(r.count ?? 0)} moved` };
    const verb = VERB[r.kind as keyof typeof VERB];
    const lead = r.client ? `${r.client} ${verb}` : verb.charAt(0).toUpperCase() + verb.slice(1);
    const n = r.count;
    const meta =
      r.kind === 'recall' && n !== null ? `${fmtCount(n)} ${n === 1 ? 'memory' : 'memories'}` : r.kind === 'store' && n !== null ? `${fmtCount(n)} ${n === 1 ? 'link' : 'links'}` : '';
    return { at, kind: r.kind, lead, text: r.summary ?? 'Summary dropped', meta };
  }
  const days = $derived.by(() => {
    const groups: { title: string; lines: Line[] }[] = [];
    const today = new Date().toDateString();
    for (const l of (data.data?.rows ?? []).map(line).sort((a, b) => b.at - a.at)) {
      const d = new Date(l.at);
      const title = d.toDateString() === today ? 'Today' : d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
      const g = groups.at(-1);
      if (g && g.title === title) g.lines.push(l);
      else groups.push({ title, lines: [l] });
    }
    return groups;
  });
  const time = (t: number) => new Date(t).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
</script>

<Screen title="Timeline" loose>
  <PageHeader title="Timeline" />
  <div role="toolbar" aria-label="Filters" class="toolbar"><ScopeChip /></div>

  <section aria-label="Memories over time" class="card chart-card">
    <div class="chart-head">
      <div role="group" aria-label="Range" class="segmented">
        {#each RANGES as r (r.key)}
          <button type="button" aria-pressed={range === r.key} onclick={() => (range = r.key)}>{r.label}</button>
        {/each}
      </div>
      <div class="legend">
        <span><span class="sq full"></span>Full</span>
        <span><span class="sq summary"></span>Summary</span>
        <span><span class="sq ghost"></span>Ghost</span>
        <span><span class="sq recalls"></span>Memories recalled</span>
      </div>
    </div>
    <div class="captions"><span>Stored each {monthly ? 'month' : 'week'}, by phase today</span><span>Memories recalled each {monthly ? 'month' : 'week'} ↓</span></div>
    {#if data.isPending}
      <div class="skeleton chart-skel"></div>
    {:else}
      <svg viewBox="0 0 {W} {H}" width="100%" height={H} role="img" aria-label="Memories stored and recalled per {monthly ? 'month' : 'week'}" preserveAspectRatio="none">
        <line class="axis" x1="0" x2={W} y1={AXIS} y2={AXIS} />
        {#each buckets as b, i (b.t)}
          {@const bx = x(i) ?? 0}
          {@const bw = x.bandwidth()}
          {@const hf = up(b.full)}
          {@const hs = up(b.summary)}
          {@const hg = up(b.ghost)}
          <g>
            <title>{fmt(b.t)}: {b.full + b.summary + b.ghost} stored, {b.recalls} recalled</title>
            <rect class="full" x={bx} y={AXIS - hf} width={bw} height={hf} />
            <rect class="summary" x={bx} y={AXIS - hf - hs} width={bw} height={hs} />
            <rect class="ghost" x={bx} y={AXIS - hf - hs - hg} width={bw} height={hg} />
            <rect class="recalls" x={bx} y={AXIS + 2} width={bw} height={down(b.recalls)} />
          </g>
        {/each}
        {#each ticks as b (b.t)}
          <text class="tick" x={(x(buckets.indexOf(b)) ?? 0) + x.bandwidth() / 2} y={H - 4} text-anchor="middle">{fmt(b.t)}</text>
        {/each}
      </svg>
    {/if}
  </section>

  <section aria-label="Activity" class="stream">
    {#if data.isPending}
      {#each [0, 1, 2, 3] as i (i)}<div class="skeleton row-skel"></div>{/each}
    {:else if data.isError}
      <LoadFailed onretry={() => data.refetch()} />
    {:else if !days.length}
      <div class="empty"><p>No activity yet.</p></div>
    {/if}
    {#each days as d, i (i)}
      <div class="day">
        <h2 class="card-title">{d.title}</h2>
        {#each d.lines as l, i (l.at + l.kind + i)}
          <div class="line">
            <span class="mono t">{time(l.at)}</span>
            <span class="dot {l.kind}"></span>
            {#if l.kind === 'sweep'}
              <span class="sweep-text">{l.lead}</span>
            {:else}
              <span class="what"><span class="muted">{l.lead}</span> {truncateWords(l.text, 90)}</span>
            {/if}
            <span class="meta">{l.meta}</span>
          </div>
        {/each}
      </div>
    {/each}
  </section>
</Screen>

<style>
  .toolbar {
    display: flex;
    gap: var(--space-2);
    margin-top: calc(var(--space-1) * -1);
  }
  .chart-card {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-5) var(--card-pad) var(--space-4);
  }
  .chart-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .segmented {
    display: flex;
    padding: calc(var(--hairline) * 3);
    border-radius: calc(var(--radius-md) + var(--hairline) * 2);
    background: var(--color-accent-soft);
  }
  .segmented button {
    height: calc(var(--control-h) - var(--space-1));
    padding: 0 var(--space-3);
    border: 0;
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--color-text);
    font: var(--weight-medium) var(--text-control) var(--font-sans);
    cursor: pointer;
  }
  .segmented button[aria-pressed='true'] {
    background: var(--color-surface);
  }
  .legend,
  .captions {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
    font-size: var(--text-small);
    color: var(--color-text-muted);
  }
  .captions {
    justify-content: space-between;
  }
  .legend span {
    display: flex;
    align-items: center;
    gap: calc(var(--space-1) * 1.5);
  }
  .sq {
    width: calc(var(--swatch) + var(--hairline) * 2);
    height: calc(var(--swatch) + var(--hairline) * 2);
    border-radius: calc(var(--radius-sm) / 2);
  }
  .sq.full {
    background: var(--color-node);
  }
  .sq.summary {
    background: var(--color-node-summary);
  }
  .sq.ghost {
    background: var(--color-node-ghost);
  }
  .sq.recalls {
    background: var(--color-moss);
  }
  svg rect.full {
    fill: var(--color-node);
  }
  svg rect.summary {
    fill: var(--color-node-summary);
  }
  svg rect.ghost {
    fill: var(--color-node-ghost);
  }
  svg rect.recalls {
    fill: var(--color-moss);
  }
  .axis {
    stroke: var(--color-grid);
    vector-effect: non-scaling-stroke;
  }
  .tick {
    font: var(--text-chart) var(--font-mono);
    fill: var(--color-text-axis);
  }
  .chart-skel {
    height: calc(var(--palette-top) * 2.3);
  }
  .stream {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: calc(var(--space-4) + var(--space-1) / 2);
  }
  .day {
    display: flex;
    flex-direction: column;
  }
  .day .card-title {
    margin-bottom: calc(var(--space-1) * 1.5);
  }
  .line {
    display: grid;
    grid-template-columns: calc(var(--space-8) * 1.75) var(--space-3) 1fr auto;
    align-items: center;
    gap: var(--space-3);
    padding: calc(var(--space-2) + var(--hairline)) 0;
    border-bottom: var(--hairline) solid var(--color-divider);
  }
  .t {
    font-size: var(--text-value);
    color: var(--color-text-muted);
  }
  .dot {
    width: calc(var(--dot) + var(--hairline));
    height: calc(var(--dot) + var(--hairline));
    border-radius: var(--radius-pill);
  }
  .dot {
    background: var(--color-accent);
  }
  .dot.store {
    background: transparent;
    box-shadow: inset 0 0 0 var(--rule) var(--color-accent);
  }
  .dot.sweep {
    background: transparent;
    box-shadow: inset 0 0 0 var(--rule) var(--color-text-muted);
  }
  .what {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .sweep-text {
    color: var(--color-text-2);
  }
  .meta,
  .muted {
    color: var(--color-text-muted);
  }
  .meta {
    font-size: var(--text-small);
  }
  .row-skel {
    height: var(--space-6);
  }
</style>
