<script lang="ts">
  import { createQuery, useQueryClient } from '@tanstack/svelte-query';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import { scope } from '$lib/scope.svelte';
  import { get } from '$lib/api/client';
  import { memoryPage, reinforce } from '$lib/api/memories';
  import type { HealthReport, NamespaceInfo, NamespaceStats, SystemStatus } from '$lib/api/types';
  import { bytes, count, duration, inFuture, phaseName, ago } from '$lib/format';
  import { namespacesQuery } from '$lib/queries';
  import { DAY_MS } from '$lib/fsrs';

  const client = useQueryClient();
  const report = createQuery(() => ({
    queryKey: ['health', scope.current],
    queryFn: () => get<HealthReport>('/health', { namespace: scope.current }),
    refetchInterval: 60_000,
  }));
  const system = createQuery(() => ({ queryKey: ['system'], queryFn: () => get<SystemStatus>('/system'), refetchInterval: 15_000 }));
  const namespaces = namespacesQuery();
  // Tombstones are reported per namespace by namespace_stats.
  const stats = createQuery(() => ({
    queryKey: ['ns-stats', scope.current, (namespaces.data ?? []).map((n: NamespaceInfo) => n.name).join(',')],
    queryFn: async () => {
      const names = scope.current ? [scope.current] : (namespaces.data ?? []).map((n: NamespaceInfo) => n.name);
      return Promise.all(names.map((n: string) => get<NamespaceStats>(`/namespaces/${encodeURIComponent(n)}/stats`)));
    },
    enabled: !!namespaces.data,
  }));
  const fading = createQuery(() => ({
    queryKey: ['fading', scope.current],
    queryFn: () => memoryPage({ namespace: scope.current, phases: [1, 2, 3], tags: [], entities: [], sort: 'nextChange', desc: false }, 0, 5),
  }));

  const o = $derived(report.data?.overview);
  const tombstones = $derived((stats.data ?? []).reduce((a, s) => a + s.tombstoneCount, 0));
  const f = $derived(report.data?.decayForecast);
  const st = $derived(report.data?.storage);
  const vectors = $derived((st?.vectorFiles ?? []).reduce((a, v) => a + v.bytes, 0));
  const s = $derived(system.data);
  const since = (t: number) => new Date(t).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
  const uptime = (secs: number) => {
    const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60);
    return h >= 48 ? `${Math.floor(h / 24)} d ${h % 24} h` : h > 0 ? `${h} h ${m} m` : `${m} m`;
  };
  const rows = $derived(
    f
      ? [
          { label: ['Full', 'Summary'], v: [f.transitions7d.fullToSummary, f.transitions30d.fullToSummary, f.transitions90d.fullToSummary] },
          { label: ['Summary', 'Ghost'], v: [f.transitions7d.summaryToGhost, f.transitions30d.summaryToGhost, f.transitions90d.summaryToGhost] },
          { label: ['Ghost', 'Removed'], v: [f.transitions7d.ghostToDeleted, f.transitions30d.ghostToDeleted, f.transitions90d.ghostToDeleted] },
        ]
      : [],
  );
  let reinforcing = $state<string | null>(null);
  async function doReinforce(id: string, namespace: string) {
    reinforcing = id;
    try {
      await reinforce(id, namespace, 3);
      await client.invalidateQueries();
    } finally {
      reinforcing = null;
    }
  }
</script>

<Screen gap="var(--space-5)">
  <PageHeader title="Health" />
  <div role="toolbar" aria-label="Filters" class="toolbar"><ScopeChip /></div>

  <div class="row">
    <section aria-label="Phases" class="card narrow">
      <div class="head"><h2 class="card-title">Memories</h2><span class="mono meta">{scope.label}</span></div>
      {#if o}
        <span class="display">{count(o.totalMemories)}</span>
        <div class="phasebar" aria-hidden="true">
          <span class="full" style:flex-grow={o.phaseCounts.full}></span>
          <span class="summary" style:flex-grow={o.phaseCounts.summary}></span>
          <span class="ghost" style:flex-grow={o.phaseCounts.ghost}></span>
          <span class="tomb" style:flex-grow={tombstones}></span>
        </div>
        <dl class="counts">
          <div><dt>Full</dt><dd class="mono">{count(o.phaseCounts.full)}</dd></div>
          <div><dt>Summary</dt><dd class="mono">{count(o.phaseCounts.summary)}</dd></div>
          <div><dt>Ghost</dt><dd class="mono">{count(o.phaseCounts.ghost)}</dd></div>
          <div><dt>Tombstone</dt><dd class="mono">{stats.data ? count(tombstones) : '…'}</dd></div>
          <div><dt>Permastore</dt><dd class="mono">{count(o.permastoreCount)}</dd></div>
        </dl>
      {:else}
        <span class="skeleton big"></span><span class="skeleton bar"></span>
      {/if}
    </section>

    <section aria-label="Decay forecast" class="card wide">
      <div class="head wrap"><h2 class="card-title">Coming up</h2><span class="caption">Phase changes expected unless these memories are recalled</span></div>
      <table>
        <thead><tr><th scope="col"><span class="visually-hidden">Change</span></th><th scope="col">7 days</th><th scope="col">30 days</th><th scope="col">90 days</th></tr></thead>
        <tbody>
          {#each rows as r (r.label[0])}
            <tr>
              <th scope="row">{r.label[0]} <span class="muted">→</span> {r.label[1]}</th>
              {#each r.v as n, i (i)}<td class="mono num" class:zero={n === 0}>{count(n)}</td>{/each}
            </tr>
          {:else}
            {#each [0, 1, 2] as i (i)}<tr><td colspan="4"><span class="skeleton bar"></span></td></tr>{/each}
          {/each}
        </tbody>
      </table>
    </section>
  </div>

  <div class="row">
    <section aria-label="Fading soon" class="card wide fading">
      <div class="head"><h2 class="card-title">Fading soon</h2><a href="/explore?sort=nextChange">See all in Explore</a></div>
      {#each fading.data?.rows ?? [] as m (m.id)}
        <div class="fade-row">
          <span class="fade-text" class:ghost={m.summary === null}>{m.summary ?? 'Summary dropped'}</span>
          <span class="caption nowrap">{m.nextChange ? `${phaseName(m.nextChange.to) === 'Tombstone' ? 'Removed' : phaseName(m.nextChange.to)} ${m.nextChange.at <= Date.now() ? 'at the next sweep' : `in ${duration((m.nextChange.at - Date.now()) / DAY_MS)}`}` : '—'}</span>
          <button type="button" class="small-btn" disabled={reinforcing === m.id} onclick={() => doReinforce(m.id, m.namespace)}>{reinforcing === m.id ? 'Reinforcing…' : 'Reinforce'}</button>
        </div>
      {:else}
        {#if fading.isPending}{#each [0, 1, 2] as i (i)}<span class="skeleton bar"></span>{/each}{:else}<p class="muted">Nothing is about to change phase.</p>{/if}
      {/each}
    </section>

    <section aria-label="Storage" class="card narrow">
      <div class="head"><h2 class="card-title">Storage</h2><span class="mono strong">{st ? bytes(st.totalBytes) : ''}</span></div>
      {#if st}
        <div class="phasebar" aria-hidden="true">
          <span class="s-meta" style:flex-grow={st.metaDbBytes}></span>
          <span class="s-vec" style:flex-grow={vectors}></span>
          <span class="s-edges" style:flex-grow={st.edgesDbBytes}></span>
          <span class="s-text" style:flex-grow={st.textLogBytes}></span>
        </div>
        <dl class="list">
          <div><dt><span class="key s-meta"></span>meta.db</dt><dd class="mono">{bytes(st.metaDbBytes)}</dd></div>
          <div><dt><span class="key s-vec"></span>Vectors</dt><dd class="mono">{bytes(vectors)}</dd></div>
          <div><dt><span class="key s-edges"></span>edges.db</dt><dd class="mono">{bytes(st.edgesDbBytes)}</dd></div>
          <div><dt><span class="key s-text"></span>Text log</dt><dd class="mono">{bytes(st.textLogBytes)}</dd></div>
        </dl>
      {:else}
        <span class="skeleton bar"></span>
      {/if}
    </section>
  </div>

  <section aria-label="System" class="card">
    <div class="head wrap"><h2 class="card-title">System</h2></div>
    {#if s}
      <div class="tiles">
        <div class="tile"><span class="tl">Version</span><span class="tv">{s.version}</span><span class="tc">daemon · protocol v{s.protocolVersion}</span></div>
        <div class="tile"><span class="tl">Uptime</span><span class="tv">{uptime(s.uptimeSecs)}</span><span class="tc">since {since(s.startedAt)}</span></div>
        <div class="tile"><span class="tl">Memory</span><span class="tv">{s.process.rssBytes !== null ? bytes(s.process.rssBytes) : '—'}</span><span class="tc">resident</span></div>
        <div class="tile"><span class="tl">Embedding</span><span class="tv">{s.embedding.latencyMs ? `${Math.round(s.embedding.latencyMs.smoothed)} ms` : '—'}</span><span class="tc mono">{s.embedding.model}</span></div>
      </div>
      <div class="sys-row">
        <div class="subsystems" aria-label="Subsystems">
          <span class="micro-label">Subsystems</span>
          {#each s.health.subsystems as sub (sub.name)}
            <span class="sub" title={sub.message ?? ''}><span class="sdot" class:down={sub.status !== 'ok' && sub.status !== 'up'}></span>{sub.name}</span>
          {/each}
        </div>
        <div class="agents" aria-label="Connected agents">
          <span class="micro-label">Connected</span>
          {#each s.clients.filter((c) => c.kind !== 'ui') as c (c.id)}
            <span class="agent">{c.agent ?? 'unknown agent'} <span class="muted mono">{ago(c.since)}</span></span>
          {:else}
            <span class="muted">No agents connected</span>
          {/each}
        </div>
        <span class="caption">{s.sweep.enabled ? (s.sweep.nextAt ? `Next decay sweep ${inFuture(s.sweep.nextAt)}` : `Decay sweeps every ${duration(s.sweep.intervalSecs / 86400)}`) : 'Decay sweeps are disabled'}</span>
      </div>
    {:else}
      <span class="skeleton bar"></span>
    {/if}
  </section>
</Screen>

<style>
  .toolbar {
    display: flex;
    gap: var(--space-2);
    margin-top: calc(var(--space-1) * -1);
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
  }
  .card {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    min-width: 0;
  }
  .narrow {
    flex: 1 1 calc(var(--panel-width) - var(--space-8) * 2);
  }
  .wide {
    flex: 2 1 var(--main-min);
    gap: var(--space-3);
  }
  .head {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .head.wrap {
    flex-wrap: wrap;
  }
  .meta,
  .caption,
  .muted {
    color: var(--color-text-muted);
  }
  .meta {
    font-size: var(--text-value);
  }
  .strong {
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .caption {
    font-size: var(--text-control);
  }
  .nowrap {
    white-space: nowrap;
  }
  .display {
    font: var(--weight-regular) var(--text-display) / 1 var(--font-serif);
    letter-spacing: -0.02em;
  }
  .phasebar {
    display: flex;
    gap: calc(var(--hairline) * 2);
    height: calc(var(--swatch) + var(--hairline) * 2);
    border-radius: var(--radius-pill);
    overflow: hidden;
  }
  .phasebar span {
    flex: 0 1 0;
  }
  .phasebar .full {
    background: var(--color-node);
  }
  .phasebar .summary {
    background: var(--color-node-summary);
    opacity: 0.6;
  }
  .phasebar .ghost {
    border: var(--hairline) solid var(--color-node-ghost);
  }
  .phasebar .tomb,
  .s-text {
    background: var(--color-grid);
  }
  .s-meta {
    background: var(--color-node);
  }
  .s-vec {
    background: var(--color-accent);
  }
  .s-edges {
    background: var(--color-agent-2);
  }
  dl {
    margin: 0;
  }
  .counts {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: var(--space-2) var(--space-4);
    font-size: var(--text-button);
  }
  .counts div,
  .list div {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  dt {
    color: var(--color-text-muted);
  }
  dd {
    margin: 0;
  }
  .list {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    font-size: var(--text-button);
  }
  .list dt {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--color-text-2);
  }
  .list dd {
    color: var(--color-text-muted);
  }
  .key {
    width: var(--swatch);
    height: var(--swatch);
    border-radius: calc(var(--radius-sm) / 2);
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-body);
  }
  thead th {
    padding-bottom: var(--space-2);
    text-align: right;
    width: 22%;
    font: var(--weight-medium) var(--text-label) var(--font-mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-text-muted);
  }
  tbody tr {
    border-top: var(--hairline) solid var(--color-divider);
  }
  tbody th {
    padding: var(--space-3) 0;
    text-align: left;
    font-weight: var(--weight-regular);
    color: var(--color-text-2);
  }
  .num {
    padding: var(--space-3) 0;
    text-align: right;
    font-size: calc(var(--text-body) + var(--hairline) * 1.5);
    color: var(--color-text);
  }
  .num.zero {
    color: var(--color-edge);
  }
  .fading {
    padding-bottom: var(--space-3);
    gap: 0;
  }
  .fading .head {
    padding-bottom: calc(var(--space-1) * 1.5);
  }
  .fading a {
    font-size: var(--text-control);
    text-underline-offset: 0.2em;
  }
  .fade-row {
    display: grid;
    grid-template-columns: 1fr auto auto;
    align-items: center;
    gap: var(--space-4);
    padding: calc(var(--space-2) + var(--space-1) / 2) 0;
    border-bottom: var(--hairline) solid var(--color-divider);
  }
  .fade-text {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .fade-text.ghost {
    font-style: italic;
    color: var(--color-text-muted);
  }
  .small-btn {
    height: calc(var(--control-h) - var(--hairline) * 2);
    padding: 0 var(--space-3);
    border: var(--hairline) solid var(--color-accent-soft-border);
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--color-accent-strong);
    font: var(--weight-medium) var(--text-small) var(--font-sans);
    cursor: pointer;
  }
  .tiles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(calc(var(--sidebar-min) - var(--space-2)), 100%), 1fr));
    gap: calc(var(--space-2) + var(--space-1) / 2);
  }
  .tile {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--tile-pad);
    border-radius: var(--radius-md);
    background: var(--color-sunken);
  }
  .tl,
  .tc {
    font-size: var(--text-small);
    color: var(--color-text-muted);
  }
  .tv {
    font: var(--weight-regular) var(--text-tile) / 1.1 var(--font-serif);
  }
  .sys-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  .subsystems,
  .agents {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--text-control);
  }
  .sub {
    display: flex;
    align-items: center;
    gap: calc(var(--space-1) * 1.5);
  }
  .sdot {
    width: var(--dot);
    height: var(--dot);
    border-radius: var(--radius-pill);
    background: var(--color-accent);
  }
  .sdot.down {
    background: var(--color-text-muted);
  }
  .skeleton.big {
    height: var(--text-display);
    width: 40%;
  }
  .skeleton.bar {
    display: block;
    height: var(--space-3);
    margin: var(--space-2) 0;
  }
</style>
