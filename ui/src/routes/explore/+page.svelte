<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<script lang="ts">
  import { untrack } from 'svelte';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { createQueries, createQuery } from '@tanstack/svelte-query';
  import { createVirtualizer } from '@tanstack/svelte-virtual';
  import { DropdownMenu } from 'bits-ui';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import FilterMenu from '$lib/components/FilterMenu.svelte';
  import MemoryPanel from '$lib/components/MemoryPanel.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import LoadFailed from '$lib/components/LoadFailed.svelte';
  import Loading from '$lib/components/Loading.svelte';
  import { scope } from '$lib/scope.svelte';
  import { selection } from '$lib/selection.svelte';
  import { facets as fetchFacets, memoryPage, PAGE, type PageQuery, type SortKey } from '$lib/api/memories';
  import type { MemoryRow, Phase } from '$lib/api/shapes';
  import { ago, count, duration, ms, phaseLabel } from '$lib/format';
  import { DAY_MS } from '$lib/fsrs';
  import { storeKey } from '$lib/queries';

  // ── URL state (sort and filter state lives in the URL) ───────
  const SORTS: { key: SortKey; label: string; desc: boolean }[] = [
    { key: 'lastAccess', label: 'Last access', desc: true },
    { key: 'strength', label: 'Strength', desc: false },
    { key: 'nextChange', label: 'Next change', desc: false },
    { key: 'created', label: 'Created', desc: true },
    { key: 'summary', label: 'Memory', desc: false },
  ];
  const params = $derived(page.url.searchParams);
  // /explore?id=<id> (a search hit): that memory opens above the list, whatever the filters and the loaded rows hold.
  const hit = $derived(params.get('id'));
  $effect(() => {
    const id = hit;
    if (id) untrack(() => selection.select(id));
  });
  /** Selects in the list; the search hit's place above it ends. */
  function choose(id: string | null) {
    selection.select(id);
    if (hit) setParams((q) => q.delete('id'));
  }
  const ALL: Phase[] = ['full', 'summary', 'ghost'];
  const phases = $derived(params.has('phase') ? ALL.filter((p) => params.getAll('phase').includes(p)) : ALL);
  const tagSel = $derived(params.getAll('tag'));
  const entitySel = $derived(params.getAll('entity'));
  // The agent is the client that saved the memory (GET /v1/ui/facets).
  const clientSel = $derived(params.getAll('client'));
  const sort = $derived((params.get('sort') as SortKey) ?? 'lastAccess');
  const desc = $derived(params.has('dir') ? params.get('dir') === 'desc' : SORTS.find((s) => s.key === sort)!.desc);

  function setParams(update: (p: URLSearchParams) => void) {
    const next = new URLSearchParams(page.url.search);
    update(next);
    goto(`?${next}`, { replace: true, reset: false });
  }
  const togglePhase = (p: Phase) =>
    setParams((q) => {
      const cur = phases.includes(p) ? phases.filter((x) => x !== p) : ALL.filter((x) => x === p || phases.includes(x));
      q.delete('phase');
      cur.forEach((x) => q.append('phase', x));
      if (cur.length === 3) q.delete('phase');
      if (!cur.length) q.append('phase', 'none');
    });
  const filtered = $derived(phases.length < 3 || tagSel.length > 0 || entitySel.length > 0 || clientSel.length > 0);
  const clearFilters = () =>
    setParams((q) => ['phase', 'tag', 'entity', 'client'].forEach((k) => q.delete(k)));
  const setList = (key: string, values: string[]) =>
    setParams((q) => {
      q.delete(key);
      values.forEach((v) => q.append(key, v));
    });

  const query: PageQuery = $derived({ namespace: scope.current, phases, tags: tagSel, entities: entitySel, clients: clientSel, sort, desc });
  const key = $derived(JSON.stringify(query));

  // ── Rows: fetched in pages as the reader reaches them ─────────────────
  const first = createQuery(() => ({
    queryKey: storeKey('memories', key, 0),
    // No phase chip on: nothing to ask for.
    queryFn: () => (phases.length ? memoryPage(query, 0) : Promise.resolve({ total: 0, offset: 0, rows: [] })),
    placeholderData: (prev) => prev,
  }));
  const total = $derived(first.data?.total ?? 0);
  // Later pages are queries like the first: the same retries, and refreshed on
  // focus and after the user acts. They are asked for as the reader reaches them.
  let wanted = $state<number[]>([]);
  const asked = new Set<string>();
  $effect(() => {
    key;
    wanted = [];
    asked.clear();
  });
  const later = createQueries(() => ({
    queries: wanted.map((p) => ({ queryKey: storeKey('memories', key, p), queryFn: () => memoryPage(query, p * PAGE) })),
  }));
  const laterOf = (p: number) => later[wanted.indexOf(p)];
  const pages = $derived.by(() => {
    const m = new Map<number, MemoryRow[]>();
    if (first.data) m.set(0, first.data.rows);
    wanted.forEach((p, i) => later[i]?.data && m.set(p, later[i].data.rows));
    return m;
  });
  function rowAt(i: number): MemoryRow | undefined {
    const p = Math.floor(i / PAGE);
    const rows = pages.get(p);
    if (rows) return rows[i - p * PAGE];
    const k = `${key}:${p}`;
    if (p > 0 && !asked.has(k)) {
      asked.add(k);
      // Not during rendering: the template reads this.
      const forKey = key;
      queueMicrotask(() => forKey === key && !wanted.includes(p) && (wanted = [...wanted, p]));
    }
    return undefined;
  }

  let scroller: HTMLDivElement | undefined = $state();
  const virtualizer = createVirtualizer<HTMLDivElement, HTMLTableSectionElement>({
    count: 0,
    getScrollElement: () => scroller ?? null,
    estimateSize: () => 44,
    overscan: 12,
  });
  $effect(() => {
    const count = total;
    const el = scroller;
    // setOptions notifies the store; don't let this effect depend on it.
    untrack(() => $virtualizer.setOptions({ count, getScrollElement: () => el ?? null }));
  });
  const items = $derived($virtualizer.getVirtualItems());
  // Rows are measured (an expanded row is taller); the virtualizer keeps
  // watching each one, so a detail that finishes loading re-measures itself.
  function measure(node: HTMLTableSectionElement) {
    $virtualizer.measureElement(node);
  }
  /** Clicking a row expands it in place; clicking it again collapses it. */
  const toggle = (id: string) => choose(selection.id === id ? null : id);
  const padTop = $derived(items.length ? items[0].start : 0);
  const padBottom = $derived(items.length ? $virtualizer.getTotalSize() - items[items.length - 1].end : 0);

  // ── Filters' options ──────────────────────────────────────────────────
  const facetList = createQuery(() => ({ queryKey: storeKey('facets', scope.current), queryFn: () => fetchFacets(scope.current) }));

  // ── Selection and keyboard ────────────────────────────────────────────
  const selectedIndex = $derived.by(() => {
    if (!selection.id) return -1;
    for (const [p, rows] of pages) {
      const i = rows.findIndex((r) => r.id === selection.id);
      if (i >= 0) return p * PAGE + i;
    }
    return -1;
  });
  // A memory selected from elsewhere (the map) scrolls into view and opens; a search hit shows above the list.
  $effect(() => {
    const i = selectedIndex;
    if (i >= 0 && selection.id !== hit) untrack(() => $virtualizer.scrollToIndex(i, { align: 'auto' }));
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && selection.id) {
      choose(null);
      return;
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const target = e.target as HTMLElement;
    if (target.tagName === 'INPUT') return;
    e.preventDefault();
    const next = Math.min(total - 1, Math.max(0, selectedIndex + (e.key === 'ArrowDown' ? 1 : -1)));
    const row = rowAt(next);
    if (row) {
      choose(row.id);
      $virtualizer.scrollToIndex(next, { align: 'auto' });
      requestAnimationFrame(() => scroller?.querySelector<HTMLButtonElement>(`[data-id="${row.id}"]`)?.focus());
    }
  }

  const now = Date.now();
  function next(m: MemoryRow) {
    if (!m.nextChange) return m.permastore ? 'Permastore' : '—';
    const days = (ms(m.nextChange.at) - now) / DAY_MS;
    return `${phaseLabel(m.nextChange.to)} ${days <= 0 ? 'next sweep' : `in ${duration(days)}`}`;
  }
</script>

<svelte:window {onkeydown} />

<Screen title="Explore">
  <PageHeader title="Explore" meta={first.data ? `${count(total)} ${total === 1 ? 'memory' : 'memories'}` : undefined} />

  <div role="toolbar" aria-label="Filters" class="toolbar">
    <ScopeChip />
    <span class="toolbar-divider"></span>
    {#each ALL as p (p)}
      <button type="button" class="chip" aria-pressed={phases.includes(p)} onclick={() => togglePhase(p)}>{phaseLabel(p)}</button>
    {/each}
    <span class="toolbar-divider"></span>
    <FilterMenu label="Tags" options={(facetList.data?.tags ?? []).map((t) => ({ value: t.name, count: t.count }))}
      bind:selected={() => tagSel, (v) => setList('tag', v)} />
    <FilterMenu label="Entities" options={(facetList.data?.entities ?? []).map((t) => ({ value: t.name, count: t.count }))}
      bind:selected={() => entitySel, (v) => setList('entity', v)} />
    <FilterMenu label="Agent" options={(facetList.data?.clients ?? []).map((t) => ({ value: t.name, count: t.count }))} empty="No memory names the agent that saved it yet"
      bind:selected={() => clientSel, (v) => setList('client', v)} />
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class="chip sort-chip"><span class="sort-label">Sort</span> {SORTS.find((s) => s.key === sort)?.label}<Icon name="chevron" stroke={1.8} size="var(--icon-sm)" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class="menu" sideOffset={6} align="end">
          <DropdownMenu.RadioGroup value={sort} onValueChange={(v) => setParams((q) => { q.set('sort', v); q.delete('dir'); })}>
            {#each SORTS as s (s.key)}<DropdownMenu.RadioItem class="menu-item" value={s.key}>{s.label}</DropdownMenu.RadioItem>{/each}
          </DropdownMenu.RadioGroup>
          <DropdownMenu.Separator class="menu-sep" />
          <DropdownMenu.Item class="menu-item" onSelect={() => setParams((q) => q.set('dir', desc ? 'asc' : 'desc'))}>{desc ? 'Descending' : 'Ascending'} · reverse</DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  </div>

  {#if selection.id && (selection.id === hit || selectedIndex < 0) && !first.isPending}
    <!-- A search hit, or a selection that is not among the loaded rows: shown above the table. -->
    <div class="pinned">
      <MemoryPanel id={selection.id} layout="inline" heading onclose={() => choose(null)} onopen={(id) => choose(id)}
        secondary={{ label: 'Show on map', onclick: () => goto(`/map?id=${selection.id}`) }} />
    </div>
  {/if}

  {#if first.isPending && !first.data}
    <Loading />
  {:else if first.isError && !first.data}
    <LoadFailed error={first.error} onretry={() => first.refetch()} />
  {:else if total === 0}
    <div class="empty">
      {#if filtered}
        <p>No memories match these filters.</p><button type="button" class="btn-secondary" onclick={clearFilters}>Clear filters</button>
      {:else}
        <p>No memories in this store yet. Agents add them with store_memory.</p><a class="btn-secondary" href="/setup">Set up an agent</a>
      {/if}
    </div>
  {:else}
  <div class="table-wrap" bind:this={scroller}>
    <table>
      <thead>
        <tr>
          <th scope="col" class="first">Memory</th>
          <th scope="col" class="w-phase">Phase</th>
          <th scope="col" class="w-strength">Strength</th>
          <th scope="col" class="w-next">Next change</th>
          <th scope="col" class="w-ns">Namespace</th>
          <th scope="col" class="w-last last">Accessed</th>
        </tr>
      </thead>
      {#if first.isPending}
        <tbody>
          {#each Array(12) as _, i (i)}
            <tr class="mrow"><td colspan="6" class="skel-cell"><span class="skeleton bar"></span></td></tr>
          {/each}
        </tbody>
      {:else}
        {#if padTop > 0}<tbody aria-hidden="true"><tr style:height="{padTop}px"></tr></tbody>{/if}
        {#each items as item (item.key)}
          {@const m = rowAt(item.index)}
          {@const open = !!m && m.id === selection.id && m.id !== hit}
          <!-- One tbody per row so the virtualizer can measure an expanded row. -->
          <tbody data-index={item.index} use:measure>
            <!-- The whole row is a click target; the summary button is the keyboard path. -->
            <tr class="mrow" class:selected={open} onclick={() => m && toggle(m.id)}>
              {#if m}
                {@const r = m.strength}
                <td class="first cell-summary">
                  <button type="button" data-id={m.id} aria-expanded={open} class:ghost={m.summary === null} onclick={(e) => { e.stopPropagation(); toggle(m.id); }}>{m.summary ?? 'Summary dropped'}</button>
                </td>
                <td><span class="phase"><span class="ph ph-{m.phase}"></span>{phaseLabel(m.phase)}</span></td>
                <td><span class="strength"><span class="track"><span class="fill" style:width="{Math.round(r * 100)}%"></span></span><span class="mono num">{r.toFixed(2)}</span></span></td>
                <td class="meta">{next(m)}</td>
                <td class="mono num muted">{m.namespace}</td>
                <td class="mono num muted last">{m.retrievedAt ? ago(ms(m.retrievedAt)) : 'Never'}</td>
              {:else}
                {@const p = Math.floor(item.index / PAGE)}
                {@const failed = laterOf(p)}
                {#if failed?.isError && item.index === Math.max(p * PAGE, items[0].index)}
                  <td colspan="6"><LoadFailed error={failed.error} onretry={() => failed.refetch()} /></td>
                {:else}
                  <td colspan="6" class="skel-cell"><span class="skeleton bar"></span></td>
                {/if}
              {/if}
            </tr>
            {#if open && m}
              <tr class="expanded">
                <td colspan="6">
                  <MemoryPanel id={m.id} layout="inline" onclose={() => choose(null)} onopen={(id) => choose(id)}
                    secondary={{ label: 'Show on map', onclick: () => goto(`/map?id=${m.id}`) }} />
                </td>
              </tr>
            {/if}
          </tbody>
        {/each}
        {#if padBottom > 0}<tbody aria-hidden="true"><tr style:height="{padBottom}px"></tr></tbody>{/if}
      {/if}
    </table>
  </div>
  {/if}
</Screen>


<style>
  .toolbar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  :global(.sort-chip) {
    margin-left: auto;
    border: 0 !important;
    padding: 0 var(--space-2) !important;
  }
  .sort-label {
    color: var(--color-text-muted);
    font-weight: var(--weight-regular);
  }
  .table-wrap {
    flex: 1 1 auto;
    min-height: 0;
    overflow: auto;
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-surface);
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: var(--text-body);
  }
  th {
    position: sticky;
    top: 0;
    z-index: 1;
    background: var(--color-surface);
    text-align: left;
    padding: calc(var(--space-2) + var(--space-1) / 2) var(--space-3);
    font: var(--weight-medium) var(--text-label) var(--font-mono);
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-text-muted);
    border-bottom: var(--hairline) solid var(--color-border);
    white-space: nowrap;
  }
  th.first,
  td.first {
    padding-left: var(--space-4);
  }
  th.last,
  td.last {
    text-align: right;
    padding-right: var(--space-4);
  }
  .w-phase {
    width: calc(var(--space-8) * 3.25);
  }
  .w-strength {
    width: calc(var(--space-8) * 3.75);
  }
  .w-next {
    width: calc(var(--space-8) * 4.7);
  }
  .w-ns {
    width: calc(var(--space-8) * 4);
  }
  .w-last {
    width: calc(var(--space-8) * 3);
  }
  .mrow {
    height: var(--row-h);
    border-bottom: var(--hairline) solid var(--color-divider);
    cursor: pointer;
  }
  .pinned {
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-lg);
  }
  .expanded {
    background: var(--color-accent-soft);
    border-bottom: var(--hairline) solid var(--color-divider);
  }
  .expanded td {
    padding: 0;
  }
  .mrow.selected {
    background: var(--color-accent-soft);
  }
  td {
    padding: 0 var(--space-3);
    white-space: nowrap;
  }
  td.cell-summary {
    padding: 0;
    white-space: normal;
    max-width: 0;
    width: 100%;
  }
  .cell-summary button {
    display: block;
    width: 100%;
    padding: var(--space-3) var(--space-4);
    border: 0;
    background: transparent;
    text-align: left;
    cursor: pointer;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* An opened row shows its whole summary. */
  .mrow.selected .cell-summary button {
    white-space: normal;
  }
  .cell-summary button.ghost {
    font-style: italic;
    color: var(--color-text-muted);
  }
  .phase {
    display: flex;
    align-items: center;
    gap: calc(var(--space-2) - var(--hairline));
    color: var(--color-text-2);
  }
  .ph {
    width: var(--swatch);
    height: var(--swatch);
    border-radius: var(--radius-pill);
    background: var(--color-node);
  }
  .ph-summary {
    opacity: 0.38;
  }
  .ph-ghost {
    background: transparent;
    border: var(--hairline) solid var(--color-node);
  }
  .strength {
    display: flex;
    align-items: center;
    gap: calc(var(--space-2) + var(--space-1) / 2);
  }
  .track {
    flex: 0 0 var(--strength-bar-w);
    height: var(--bar-h);
    border-radius: var(--radius-pill);
    background: var(--color-border);
  }
  .fill {
    display: block;
    height: var(--bar-h);
    border-radius: var(--radius-pill);
    background: var(--color-accent);
  }
  .num {
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .muted,
  .meta {
    color: var(--color-text-muted);
  }
  .meta {
    font-size: var(--text-control);
  }
  .skel-cell {
    padding: var(--space-3) var(--space-4);
  }
  .skeleton.bar {
    display: block;
    height: var(--space-3);
    width: 70%;
  }
</style>
