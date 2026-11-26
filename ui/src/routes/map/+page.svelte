<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<script lang="ts">
  import { palette } from '$lib/palette.svelte';
  import { fly } from 'svelte/transition';
  import { untrack } from 'svelte';
  import { page } from '$app/state';
  import { createQuery, useQueryClient } from '@tanstack/svelte-query';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { storeKey } from '$lib/queries';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import FilterMenu from '$lib/components/FilterMenu.svelte';
  import MemoryPanel from '$lib/components/MemoryPanel.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import { scope } from '$lib/scope.svelte';
  import { selection } from '$lib/selection.svelte';
  import { fetchGraph, activeSince } from '$lib/api/snapshot';
  import { phaseName, count } from '$lib/format';
  import { EDGE_LABELS, EDGE } from '$lib/map/encoding';
  import { live } from '$lib/live.svelte';
  import { ago } from '$lib/format';

  $effect(() => {
    live.start();
    return () => live.release();
  });
  let tick = $state(Date.now());
  $effect(() => {
    const t = setInterval(() => (tick = Date.now()), 15_000);
    return () => clearInterval(t);
  });
  // The recall pill: only for a recall that arrived while the page is open. Without a client name it leaves the name out.
  const pill = $derived.by(() => {
    tick;
    const r = [...live.recent].reverse().find((x) => x.event.kind === 'recall');
    if (!r || r.event.kind !== 'recall') return null;
    const e = r.event;
    return { text: `${e.client ? `${e.client} · ` : ''}recall · ${ago(e.at)}`, recent: Date.now() - e.at < 60_000, event: r };
  });

  const graph = createQuery(() => ({
    queryKey: storeKey('graph', scope.current),
    queryFn: () => fetchGraph(scope.current),
    staleTime: 60_000,
  }));
  // A store or forget event changes the counts and the namespaces' sums.
  const client = useQueryClient();
  $effect(() =>
    live.subscribe(({ event }) => {
      if (event.kind === 'store' || event.kind === 'forget' || event.kind === 'phase_change') {
        void client.invalidateQueries({ queryKey: storeKey('namespaces') });
      }
    }),
  );

  // Large stores start with the last 90 days of activity: fewer nodes keep the
  // physics calm. The Window menu widens it to everything.
  const WINDOWS = [
    { days: 7, label: '7 days' },
    { days: 30, label: '30 days' },
    { days: 90, label: '90 days' },
    { days: 365, label: '1 year' },
    { days: 0, label: 'All time' },
  ];
  let windowDays = $state(90);
  const shown = $derived(
    graph.data && windowDays > 0 ? activeSince(graph.data, graph.data.generatedAt - windowDays * 86_400_000) : graph.data,
  );

  let phases = $state(new Set([1, 2, 3]));
  const togglePhase = (p: number) => {
    const next = new Set(phases);
    if (next.has(p)) next.delete(p);
    else next.add(p);
    phases = next;
  };
  // The link filter offers the five edge types; every one but Supersedes shows by default.
  const ALL_TYPES = [EDGE.parent, EDGE.associative, EDGE.entity, EDGE.temporal, EDGE.supersedes];
  let linkSel = $state<string[]>(ALL_TYPES.filter((t) => t !== EDGE.supersedes).map(String));
  const edgeTypes = $derived(new Set(linkSel.map(Number)));
  const shownTypes = $derived(edgeTypes.size);

  let days = $state(0);

  const daysLabel = $derived(days === 0 ? 'now' : days < 60 ? `+${days} d` : `+${Math.round(days / 30)} mo`);

  let map: { zoomIn(): void; zoomOut(): void; fit(): void; focusNeighborhood(id: string): void; exitFocus(): void; center(id: string, covered?: number): void; reveal(id: string, covered: number): void; redraw(): void } | undefined = $state();
  // Height of the memory card plus its gap, so the map can keep the selection in view.
  let cardH = $state(0);
  const covered = $derived(cardH ? cardH + 72 : 0);
  const reducedMotion = typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches;
  let localGraph = $state(false);

  // ?id=… (from Explore's "Show on map" or the palette's ⌘↵): select it and
  // center on it once the layout has settled.
  let pendingCenter: string | null = null;
  let settled = false;
  $effect(() => {
    const id = page.url.searchParams.get('id');
    if (id) {
      if (graph.data?.index.has(id) && !shown?.index.has(id)) windowDays = 0;
      selection.select(id);
      if (settled) map?.center(id, untrack(() => covered) || 360);
      else pendingCenter = id;
    }
  });
  function onsettled() {
    settled = true;
    if (pendingCenter) map?.center(pendingCenter, covered || 360);
    pendingCenter = null;
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key !== 'Escape' || palette.open) return;
    if (localGraph) {
      e.preventDefault();
      map?.exitFocus();
    } else if (selection.id) {
      // Esc dismisses the memory card.
      e.preventDefault();
      selection.select(null);
    }
  }
  // Redraw the map whenever the card's size changes: Chromium can otherwise
  // keep stale map pixels around a card layered over the WebGL canvas.
  $effect(() => {
    cardH;
    map?.redraw();
  });
  // A clicked memory the card would cover slides up into view.
  $effect(() => {
    const id = selection.id;
    if (id && covered) map?.reveal(id, covered);
  });
</script>

<svelte:window {onkeydown} />

<Screen title="Memory map">
  <PageHeader
    title="Memory map"
    meta={graph.data && shown
      ? shown.ids.length === graph.data.ids.length
        ? `${count(graph.data.ids.length)} ${graph.data.ids.length === 1 ? 'memory' : 'memories'}`
        : `${count(shown.ids.length)} of ${count(graph.data.ids.length)} memories`
      : undefined}
  />

  <div role="toolbar" aria-label="Graph filters" class="toolbar">
    <ScopeChip />
    <span class="toolbar-divider"></span>
    {#each [1, 2, 3] as p (p)}
      <button type="button" class="chip" aria-pressed={phases.has(p)} onclick={() => togglePhase(p)}>{phaseName(p)}</button>
    {/each}
    <FilterMenu
      label="Links · {shownTypes} {shownTypes === 1 ? 'type' : 'types'}"
      options={ALL_TYPES.map((t) => ({ value: String(t), label: EDGE_LABELS[t] }))}
      bind:selected={linkSel}
      showCount={false}
    />
    <label class="window">
      <span>Active in</span>
      <select bind:value={windowDays} aria-label="Show memories active in">
        {#each WINDOWS as w (w.days)}<option value={w.days}>{w.label}</option>{/each}
      </select>
    </label>
    <div class="scrubber">
      <label for="decay-scrub">Decay</label>
      <input id="decay-scrub" type="range" min="0" max="365" step="1" bind:value={days} aria-valuetext={daysLabel} />
      <span class="mono when">{daysLabel}</span>
    </div>
  </div>

  <div class="canvas-card">
    {#if graph.isPending}
      <div class="map-state"><Loading /></div>
    {:else if graph.isError}
      <div class="map-state"><LoadFailed error={graph.error} onretry={() => graph.refetch()} /></div>
    {:else if graph.data && graph.data.ids.length === 0}
      <div class="map-state">
        <div class="empty">
          <p>No memories in this store yet. Agents add them with <span class="mono small">store_memory</span>.</p>
          <a class="btn-secondary" href="/setup">Set up an agent</a>
        </div>
      </div>
    {:else if shown && shown.ids.length === 0}
      <div class="map-state">
        <div class="empty"><p>No memories match these filters.</p><button type="button" class="link" onclick={() => (windowDays = 0)}>Show all time</button></div>
      </div>
    {:else if shown}
      {#if pill}
        <button type="button" class="pill" class:quiet={!pill.recent} title="Replay in slow motion" onclick={() => live.replay(pill.event)}>
          <span class="pill-dot"></span>{pill.text}
        </button>
      {/if}
      {#if localGraph}
        <button type="button" class="pill back" onclick={() => map?.exitFocus()}>← Back</button>
      {/if}
      {#key shown}
        {#await import('$lib/map/MemoryMap.svelte') then { default: MemoryMap }}
          <MemoryMap
            bind:this={map}
            snapshot={shown}
            scopeKey={scope.current}
            selectedId={selection.id}
            projectionDays={days}
            {phases}
            {edgeTypes}
            {onsettled}
            onfocuschange={(f: boolean) => (localGraph = f)}
            onselect={(id) => {
              selection.select(id);
            }}
          />
        {/await}
      {/key}
      <div class="legend" aria-label="Legend">
        <span title="Lighter means it is fading"><span class="sw full"></span>Full</span>
        <span title="Lighter means it is fading"><span class="sw summary"></span>Summary</span>
        <span><span class="sw ghost"></span>Ghost</span>
        <span><span class="sw lit"></span>Recalled</span>
        <span><span class="sw suppressed"></span>Suppressed</span>
      </div>
      <div class="zoom">
        <button type="button" class="zbtn" aria-label="Zoom in" onclick={() => map?.zoomIn()}><Icon name="plus" stroke={1.8} /></button>
        <button type="button" class="zbtn" aria-label="Zoom out" onclick={() => map?.zoomOut()}><Icon name="minus" stroke={1.8} /></button>
        <button type="button" class="zbtn" aria-label="Fit graph to view" onclick={() => map?.fit()}><Icon name="fit" stroke={1.8} /></button>
      </div>
      {#if selection.id}
        <!-- The selected memory: the same card Explore opens under a row. -->
        <div class="memory-card" bind:clientHeight={cardH} transition:fly={{ y: 24, duration: reducedMotion ? 0 : 180 }} onintroend={() => map?.redraw()} onoutroend={() => map?.redraw()}>
          <MemoryPanel id={selection.id} layout="inline" heading onclose={() => selection.select(null)} onopen={(id) => selection.select(id)}
            secondary={{ label: 'Local graph', onclick: () => map?.focusNeighborhood(selection.id!) }} />
        </div>
      {/if}
    {/if}
  </div>
</Screen>


<style>
  .toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .window {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-control);
    color: var(--color-text-muted);
  }
  .window select {
    font: inherit;
    color: var(--color-text);
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-sm);
    padding: var(--space-1) var(--space-2);
  }
  .map-state .link {
    font: inherit;
    color: var(--color-accent);
    background: none;
    border: 0;
    padding: 0;
    cursor: pointer;
    text-decoration: underline;
  }
  .scrubber {
    display: flex;
    align-items: center;
    gap: calc(var(--space-2) + var(--space-1) / 2);
    margin-left: auto;
  }
  .scrubber label {
    font-size: var(--text-control);
    color: var(--color-text-muted);
  }
  .scrubber input {
    width: calc(var(--space-8) * 3.75);
    accent-color: var(--color-accent);
  }
  .when {
    font-size: var(--text-explain);
    min-width: var(--space-8);
  }
  .canvas-card {
    position: relative;
    flex: 1 1 auto;
    min-height: calc(var(--palette-top) * 4);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-canvas);
    overflow: hidden;
  }
  .map-state {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: var(--space-8);
  }
  .pill {
    position: absolute;
    left: calc(var(--space-3) + var(--space-1) / 2);
    top: calc(var(--space-3) + var(--space-1) / 2);
    z-index: 4;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: calc(var(--control-h) - var(--space-1));
    padding: 0 var(--space-3);
    border-radius: var(--radius-pill);
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    font-size: var(--text-small);
    color: var(--color-accent-strong);
    cursor: pointer;
  }
  .pill.back {
    left: 50%;
    transform: translateX(-50%);
  }
  .pill.quiet {
    color: var(--color-text-muted);
    cursor: default;
  }
  .pill-dot {
    width: calc(var(--dot) + var(--hairline));
    height: calc(var(--dot) + var(--hairline));
    border-radius: var(--radius-pill);
    background: var(--color-accent);
    box-shadow: 0 0 0 calc(var(--hairline) * 3) var(--color-accent-soft-border);
  }
  .legend {
    position: absolute;
    left: calc(var(--space-3) + var(--space-1) / 2);
    bottom: calc(var(--space-3) + var(--space-1) / 2);
    z-index: 4;
    display: flex;
    flex-wrap: wrap;
    gap: calc(var(--space-3) + var(--space-1) / 2);
    font-size: var(--text-small);
    color: var(--color-text-muted);
    pointer-events: none;
  }
  .legend > span {
    display: flex;
    align-items: center;
    gap: calc(var(--space-1) * 1.5);
  }
  .sw {
    width: var(--swatch);
    height: var(--swatch);
    border-radius: var(--radius-pill);
  }
  /* Two-tone swatches: strong on the left, fading on the right. */
  .sw.full {
    width: calc(var(--swatch) * 2);
    background: linear-gradient(90deg, var(--color-node) 50%, var(--color-node-fading) 50%);
  }
  .sw.summary {
    width: calc(var(--swatch) * 2);
    background: linear-gradient(90deg, var(--color-node-summary) 50%, var(--color-node-summary-fading) 50%);
  }
  .sw.ghost {
    background: var(--color-node-ghost);
  }
  .sw.lit {
    background: var(--color-node-lit);
    box-shadow: inset 0 0 0 calc(var(--hairline) * 2) var(--color-live);
  }
  .sw.suppressed {
    background: var(--color-suppressed);
  }
  .zoom {
    position: absolute;
    right: calc(var(--space-3) + var(--space-1) / 2);
    bottom: calc(var(--space-3) + var(--space-1) / 2);
    z-index: 4;
    display: flex;
    gap: var(--space-1);
  }
  .zbtn {
    width: var(--icon-btn);
    height: var(--icon-btn);
    display: flex;
    align-items: center;
    justify-content: center;
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-surface);
    color: var(--color-text);
    cursor: pointer;
  }
  .memory-card {
    position: absolute;
    left: var(--space-4);
    right: var(--space-4);
    /* Above the legend and zoom controls, which stay usable. */
    bottom: calc(var(--space-4) + var(--nav-item-h) + var(--space-3));
    max-height: 50%;
    overflow-y: auto;
    z-index: 6;
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-lg);
    /* A light shadow: a large blurred one over the WebGL map made Chromium
       stop drawing the memories around the card. */
    box-shadow: var(--shadow-tooltip);
  }
</style>
