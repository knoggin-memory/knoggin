<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<script lang="ts">
  import { createQuery, useQueryClient } from '@tanstack/svelte-query';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import { scope } from '$lib/scope.svelte';
  import { get, sget } from '$lib/api/client';
  import { reinforce } from '$lib/api/memories';
  import type { AdminStatus, HealthReport, SystemStatus } from '$lib/api/shapes';
  import { ago, bytes, count, duration, inFuture, longDate, ms, phaseLabel } from '$lib/format';
  import { storeKey } from '$lib/queries';
  import { session } from '$lib/session.svelte';
  import { status } from '$lib/status.svelte';
  import { store } from '$lib/store.svelte';

  // No timers: store routes are refreshed after the user acts and when the tab regains focus.
  const client = useQueryClient();
  const report = createQuery(() => ({
    queryKey: storeKey('health', scope.current),
    queryFn: () => sget<HealthReport>('/v1/ui/health', { namespace: scope.current }, true),
  }));
  const system = createQuery(() => ({ queryKey: storeKey('system'), queryFn: () => sget<SystemStatus>('/v1/ui/system') }));
  // On a local install the card's caption is the data folder.
  const admin = createQuery(() => ({
    queryKey: ['admin-status'],
    queryFn: () => get<AdminStatus>('/v1/admin/status'),
    enabled: !!session.local,
  }));
  // Fading soon: the report's memories at risk, the first five as the prototype draws them.
  const fading = $derived(report.data?.atRisk.slice(0, 5) ?? []);

  const o = $derived(report.data?.overview);
  const f = $derived(report.data?.decayForecast);
  const st = $derived(report.data?.storage);
  const s = $derived(system.data);
  const caption = $derived(session.local ? (admin.data?.dataDir ?? '') : `${(store.id ?? '').slice(0, 8)}…`);
  const modelName = $derived(status.status?.model?.name ?? s?.model.name ?? '');
  // "since 13:39" for today, else the date.
  const since = (t: string) => {
    const d = new Date(t);
    return d.toDateString() === new Date().toDateString()
      ? d.toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' })
      : d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
  };
  const uptime = (secs: number) => {
    const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60);
    return h >= 48 ? `${Math.floor(h / 24)} d` : h > 0 ? `${h} h ${m} m` : `${m} m`;
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
  let done = $state<Set<string>>(new Set());
  let error = $state<string | null>(null);
  async function doReinforce(id: string) {
    reinforcing = id;
    error = null;
    try {
      await reinforce(id, 3);
      done = new Set([...done, id]);
      await client.invalidateQueries({ queryKey: ['store'] });
      void status.poll();
    } catch (e) {
      error = (e as Error).message;
    } finally {
      reinforcing = null;
    }
  }
</script>

<Screen title="Health" loose>
  <PageHeader title="Health" />
  <div role="toolbar" aria-label="Filters" class="toolbar"><ScopeChip /></div>

  {#if report.isError && !report.data}
    <LoadFailed error={report.error} onretry={() => report.refetch()} />
  {:else}
  <div class="row">
    <section aria-label="Phases" class="card narrow">
      <div class="head"><h2 class="card-title">Memories</h2><span class="mono meta">{scope.label}</span></div>
      {#if o}
        <span class="display">{count(o.totalMemories)}</span>
        <div class="phasebar" aria-hidden="true">
          <span class="full" style:flex-grow={o.phaseCounts.full}></span>
          <span class="summary" style:flex-grow={o.phaseCounts.summary}></span>
          <span class="ghost" style:flex-grow={o.phaseCounts.ghost}></span>
        </div>
        <dl class="counts">
          <div><dt>Full</dt><dd class="mono">{count(o.phaseCounts.full)}</dd></div>
          <div><dt>Summary</dt><dd class="mono">{count(o.phaseCounts.summary)}</dd></div>
          <div><dt>Ghost</dt><dd class="mono">{count(o.phaseCounts.ghost)}</dd></div>
          <div><dt>Permastore</dt><dd class="mono">{count(o.permastoreCount)}</dd></div>
        </dl>
      {:else}
        <span class="skeleton big"></span><span class="skeleton bar"></span>
      {/if}
    </section>

    <section aria-label="Decay forecast" class="card wide">
      <div class="head wrap"><h2 class="card-title">Coming up</h2><span class="caption">Phase changes expected unless these memories are reinforced</span></div>
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
      {#if error}<p class="field-error" role="alert">{error}</p>{/if}
      {#each fading as m (m.id)}
        {@const isDone = done.has(m.id)}
        <div class="fade-row">
          <span class="fade-text" class:ghost={m.summary === null}>{m.summary ?? 'Summary dropped'}</span>
          <span class="caption nowrap">{isDone ? 'Reinforced' : `${phaseLabel(m.to)} ${m.daysUntilChange <= 0 ? 'at the next sweep' : `in ${duration(m.daysUntilChange)}`}`}</span>
          <button type="button" class="btn-secondary btn-small" disabled={reinforcing === m.id || isDone} onclick={() => doReinforce(m.id)}>{isDone ? 'Reinforced' : 'Reinforce'}</button>
        </div>
      {:else}
        {#if report.isPending}{#each [0, 1, 2] as i (i)}<span class="skeleton bar"></span>{/each}{:else}<p class="muted">Nothing is fading.</p>{/if}
      {/each}
    </section>

    <section aria-label="Storage" class="card narrow">
      <!-- The total is text plus vectors; the last snapshot holds the same data again, so it is shown beside it and not added. -->
      <div class="head"><h2 class="card-title">Storage</h2><span class="mono strong">{st ? `${bytes(st.textBytes + st.vectorBytes)}${st.lastSnapshotBytes !== null ? ` · snapshot ${bytes(st.lastSnapshotBytes)}` : ''}` : ''}</span></div>
      {#if st}
        <div class="phasebar" aria-hidden="true">
          <span class="s-text" style:flex-grow={st.textBytes}></span>
          <span class="s-vec" style:flex-grow={st.vectorBytes}></span>
        </div>
        <dl class="list">
          <div><dt><span class="key s-text"></span>Text</dt><dd class="mono">{bytes(st.textBytes)}</dd></div>
          <div><dt><span class="key s-vec"></span>Vectors</dt><dd class="mono">{bytes(st.vectorBytes)}</dd></div>
          <div><dt>Last snapshot</dt><dd class="mono">{st.lastSnapshotBytes !== null ? bytes(st.lastSnapshotBytes) : '—'}</dd></div>
        </dl>
      {:else}
        <span class="skeleton bar"></span>
      {/if}
    </section>
  </div>

  <section aria-label="System" class="card">
    <div class="head wrap"><h2 class="card-title">System</h2><span class="mono meta">{caption}</span></div>
    {#if s}
      <div class="tiles">
        <div class="tile"><span class="tl">Version</span><span class="tv">{s.version}</span><span class="tc">{session.local ? 'daemon' : 'container build'}</span></div>
        <div class="tile"><span class="tl">Uptime</span><span class="tv">{uptime(s.uptimeSecs)}</span><span class="tc">since {since(s.startedAt)}</span></div>
        <div class="tile"><span class="tl">Memory</span><span class="tv">{s.process.rssBytes !== null ? bytes(s.process.rssBytes) : '—'}</span><span class="tc">resident</span></div>
        <div class="tile"><span class="tl">Embedding</span><span class="tv">{s.embedLatencyMs !== null ? `${Math.round(s.embedLatencyMs)} ms` : '—'}</span><span class="tc">{modelName}</span></div>
        <div class="tile"><span class="tl">Recall</span><span class="tv">{s.recallLatencyMs !== null ? `${Math.round(s.recallLatencyMs)} ms` : '—'}</span><span class="tc">latency</span></div>
      </div>
      <div class="sys-row">
        <div class="subsystems" aria-label="Subsystems">
          <span class="micro-label">Subsystems</span>
          {#each s.health.subsystems as sub (sub.name)}
            <span class="sub"><span class="sdot" class:warn={sub.status === 'degraded'}></span>{sub.name}{#if sub.status === 'degraded' && sub.message}<span class="muted"> · {sub.message}</span>{/if}</span>
          {/each}
        </div>
        <div class="agents" aria-label="Connected agents">
          <span class="micro-label">Connected</span>
          {#each s.clients as c (c.name)}
            <span class="agent">{c.name} <span class="muted mono">{ago(ms(c.lastSeenAt))}</span></span>
          {:else}
            <span class="muted">None over MCP yet</span>
          {/each}
        </div>
        {#if s.sweep.nextAt}<span class="caption">Next decay sweep {inFuture(ms(s.sweep.nextAt))}</span>{:else if s.sweep.lastAt}<span class="caption">Last decay sweep {longDate(s.sweep.lastAt)}</span>{/if}
      </div>
    {:else if system.isError}
      <LoadFailed error={system.error} onretry={() => system.refetch()} />
    {:else}
      <span class="skeleton bar"></span>
    {/if}
  </section>
  {/if}
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
  }
  .phasebar .ghost {
    background: var(--color-node-ghost);
  }
  /* Storage is not a phase, so it stays off the phase colours. */
  .s-text {
    background: var(--color-node);
  }
  .s-vec {
    background: var(--color-moss);
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
  .sdot.warn {
    background: var(--color-live);
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
