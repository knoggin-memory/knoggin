<script lang="ts">
  import { createQuery } from '@tanstack/svelte-query';
  import { scaleBand, scaleLinear } from 'd3-scale';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import { scope } from '$lib/scope.svelte';
  import { live } from '$lib/live.svelte';
  import { activity } from '$lib/api/memories';
  import type { ActivityRow, UiEvent } from '$lib/api/types';
  import { truncateWords } from '$lib/format';

  const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;
  const RANGES = [
    { key: '30', label: '30 days', days: 30 },
    { key: '90', label: '90 days', days: 90 },
    { key: '365', label: '1 year', days: 365 },
    { key: 'all', label: 'All time', days: null },
  ] as const;
  let range = $state<(typeof RANGES)[number]['key']>('90');
  const since = $derived.by(() => {
    const r = RANGES.find((x) => x.key === range)!;
    return r.days === null ? null : Date.now() - r.days * 86_400_000;
  });

  const data = createQuery(() => ({
    queryKey: ['activity', scope.current, range],
    queryFn: () => activity(scope.current, since),
    refetchInterval: 30_000,
  }));

  $effect(() => {
    live.start();
    return () => live.release();
  });

  // ── Chart: stored per bucket stacked by current phase, recalls mirrored below ──
  const W = 1000, H = 240, AXIS = 132;
  const buckets = $derived(data.data?.buckets ?? []);
  const maxUp = $derived(Math.max(1, ...buckets.map((b) => b.full + b.summary + b.ghost)));
  const maxDown = $derived(Math.max(1, ...buckets.map((b) => b.recalls)));
  const x = $derived(scaleBand<number>().domain(buckets.map((_, i) => i)).range([0, W]).paddingInner(0.25));
  const up = $derived(scaleLinear().domain([0, maxUp]).range([0, AXIS - 12]));
  const down = $derived(scaleLinear().domain([0, maxDown]).range([0, H - AXIS - 20]));
  const monthly = $derived((data.data?.bucketMs ?? 0) > 8 * 86_400_000);
  const fmt = (t: number) => new Date(t).toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
  const ticks = $derived(buckets.filter((_, i) => i % Math.max(1, Math.ceil(buckets.length / 8)) === 0));

  // ── Stream: persisted rows plus live events (which add agent and query) ──
  interface Line {
    at: number;
    kind: 'recall' | 'store' | 'reinforce' | 'sweep';
    agent: string | null;
    text: string;
    meta: string;
  }
  const liveLines = $derived(
    live.recent
      .map((r) => r.event)
      .filter((e): e is Extract<UiEvent, { kind: 'recall' | 'reinforce' | 'sweep' }> => ['recall', 'reinforce', 'sweep'].includes(e.kind))
      .filter((e) => (scope.current === null || !('namespace' in e) || e.namespace === scope.current))
      .map((e): Line => {
        if (e.kind === 'recall')
          return { at: e.at, kind: 'recall', agent: e.agent, text: e.query, meta: plural(e.returned.length, 'result') };
        if (e.kind === 'reinforce') return { at: e.at, kind: 'reinforce', agent: e.agent, text: 'a memory', meta: '' };
        const moved = e.moved.fullToSummary + e.moved.summaryToGhost + e.moved.ghostToDeleted;
        return { at: e.at, kind: 'sweep', agent: null, text: `Decay sweep moved ${moved} ${moved === 1 ? 'memory' : 'memories'}`, meta: '' };
      }),
  );
  const persisted = $derived(
    (data.data?.rows ?? []).map((r: ActivityRow): Line => ({
      at: r.at,
      kind: r.kind as Line['kind'],
      agent: null,
      text: r.kind === 'sweep' ? `Decay sweep checked ${r.count} ${r.count === 1 ? 'memory' : 'memories'}` : (r.summary ?? 'Summary dropped'),
      meta: r.kind === 'recall' ? plural(r.count, 'result') : r.kind === 'store' ? plural(r.count, 'link') : '',
    })),
  );
  const lines = $derived.by(() => {
    // A live recall and its persisted access rows share a moment; keep the live one.
    const out = [...liveLines];
    for (const p of persisted) if (!out.some((l) => l.kind === p.kind && Math.abs(l.at - p.at) < 2000)) out.push(p);
    return out.sort((a, b) => b.at - a.at);
  });
  const days = $derived.by(() => {
    const groups: { title: string; lines: Line[] }[] = [];
    const today = new Date().toDateString();
    for (const l of lines) {
      const d = new Date(l.at);
      const title = d.toDateString() === today ? 'Today' : d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
      const g = groups.at(-1);
      if (g && g.title === title) g.lines.push(l);
      else groups.push({ title, lines: [l] });
    }
    return groups;
  });
  const VERB = { recall: 'recalled', store: 'stored', reinforce: 'reinforced' } as const;
  const time = (t: number) => new Date(t).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
</script>

<Screen gap="var(--space-5)">
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
        <span><span class="sq recalls"></span>Recalls</span>
      </div>
    </div>
    <div class="captions"><span>Stored each {monthly ? 'month' : 'week'}, by phase today</span><span>Recalls each {monthly ? 'month' : 'week'} ↓</span></div>
    {#if data.isPending}
      <div class="skeleton chart-skel"></div>
    {:else}
      <svg viewBox="0 0 {W} {H}" width="100%" height={H} role="img" aria-label="Memories stored and recalled per {monthly ? 'month' : 'week'}" preserveAspectRatio="none">
        <line class="axis" x1="0" x2={W} y1={AXIS} y2={AXIS} />
        {#each buckets as b, i (b.start)}
          {@const bx = x(i) ?? 0}
          {@const bw = x.bandwidth()}
          {@const hf = up(b.full)}
          {@const hs = up(b.summary)}
          {@const hg = up(b.ghost)}
          <g>
            <title>{fmt(b.start)}: {b.full} Full, {b.summary} Summary, {b.ghost} Ghost stored; {b.recalls} recalls</title>
            <rect class="full" x={bx} y={AXIS - hf} width={bw} height={hf} />
            <rect class="summary" x={bx} y={AXIS - hf - hs} width={bw} height={hs} />
            {#if hg > 0}<rect class="ghost" x={bx + 0.5} y={AXIS - hf - hs - hg + 0.5} width={Math.max(0, bw - 1)} height={Math.max(0, hg - 1)} />{/if}
            <rect class="recalls" x={bx} y={AXIS + 2} width={bw} height={down(b.recalls)} />
          </g>
        {/each}
        {#each ticks as b (b.start)}
          <text class="tick" x={(x(buckets.indexOf(b)) ?? 0) + x.bandwidth() / 2} y={H - 4} text-anchor="middle">{fmt(b.start)}</text>
        {/each}
      </svg>
    {/if}
  </section>

  <section aria-label="Activity" class="stream">
    {#if data.isPending}
      {#each [0, 1, 2, 3] as i (i)}<div class="skeleton row-skel"></div>{/each}
    {:else if !lines.length}
      <p class="muted">No activity in this range yet. It appears as agents store and recall memories.</p>
    {/if}
    {#each days as d, i (i)}
      <div class="day">
        <h2 class="card-title">{d.title}</h2>
        {#each d.lines as l, i (l.at + l.kind + i)}
          <div class="line">
            <span class="mono t">{time(l.at)}</span>
            <span class="dot {l.kind}" style:--agent="var(--color-agent-{live.agentSlot(l.agent) + 1})"></span>
            {#if l.kind === 'sweep'}
              <span class="sweep-text">{l.text}</span>
            {:else}
              <span class="what"><span class="muted">{l.agent ?? 'An agent'} {VERB[l.kind]}</span> {truncateWords(l.text, 90)}</span>
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
    opacity: 0.6;
  }
  .sq.ghost {
    border: var(--hairline) solid var(--color-node-ghost);
  }
  .sq.recalls {
    background: var(--color-agent-2);
  }
  svg rect.full {
    fill: var(--color-node);
  }
  svg rect.summary {
    fill: var(--color-node-summary);
    fill-opacity: 0.6;
  }
  svg rect.ghost {
    fill: none;
    stroke: var(--color-node-ghost);
    vector-effect: non-scaling-stroke;
  }
  svg rect.recalls {
    fill: var(--color-agent-2);
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
  .dot.recall {
    background: var(--agent);
  }
  .dot.store,
  .dot.reinforce {
    border: var(--rule) solid var(--agent);
  }
  .dot.sweep {
    border: var(--hairline) solid var(--color-text-muted);
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
