<script lang="ts">
  import { untrack } from 'svelte';
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import { createQuery } from '@tanstack/svelte-query';
  import { createVirtualizer } from '@tanstack/svelte-virtual';
  import { DropdownMenu } from 'bits-ui';
  import Screen from '$lib/components/Screen.svelte';
  import PageHeader from '$lib/components/PageHeader.svelte';
  import ScopeChip from '$lib/components/ScopeChip.svelte';
  import FilterMenu from '$lib/components/FilterMenu.svelte';
  import MemoryPanel from '$lib/components/MemoryPanel.svelte';
  import Icon from '$lib/components/Icon.svelte';
  import { scope } from '$lib/scope.svelte';
  import { live } from '$lib/live.svelte';
  import { selection } from '$lib/selection.svelte';
  import { memoryPage, tags as fetchTags, PAGE, type PageQuery, type SortKey } from '$lib/api/memories';
  import type { MemoryRow } from '$lib/api/types';
  import { ago, count, duration, phaseName } from '$lib/format';
  import { DAY_MS, strengthAt } from '$lib/fsrs';

  // ── URL state (DESIGN §8: sort and filter state lives in the URL) ───────
  const SORTS: { key: SortKey; label: string; desc: boolean }[] = [
    { key: 'lastAccess', label: 'Last access', desc: true },
    { key: 'strength', label: 'Strength', desc: false },
    { key: 'nextChange', label: 'Next change', desc: false },
    { key: 'created', label: 'Created', desc: true },
    { key: 'summary', label: 'Memory', desc: false },
  ];
  const params = $derived(page.url.searchParams);
  const phases = $derived(params.has('phase') ? params.getAll('phase').map(Number) : [1, 2, 3]);
  const tagSel = $derived(params.getAll('tag'));
  const entitySel = $derived(params.getAll('entity'));
  const agentSel = $derived(params.getAll('agent'));
  $effect(() => {
    live.start();
    return () => live.release();
  });
  // recalld doesn't persist who touched a memory: the Agent filter covers
  // what agents did while this UI was open.
  const agentIds = $derived.by(() => {
    if (!agentSel.length) return [];
    const ids = new Set<string>();
    for (const { event: e } of live.recent) {
      if (!('agent' in e) || !e.agent || !agentSel.includes(e.agent)) continue;
      if (e.kind === 'recall') e.returned.forEach((id) => ids.add(id));
      else if ('id' in e) ids.add(e.id);
    }
    return [...ids].slice(0, 500);
  });
  const sort = $derived((params.get('sort') as SortKey) ?? 'lastAccess');
  const desc = $derived(params.has('dir') ? params.get('dir') === 'desc' : SORTS.find((s) => s.key === sort)!.desc);

  function setParams(update: (p: URLSearchParams) => void) {
    const next = new URLSearchParams(page.url.search);
    update(next);
    goto(`?${next}`, { replace: true, reset: false });
  }
  const togglePhase = (p: number) =>
    setParams((q) => {
      const cur = phases.includes(p) ? phases.filter((x) => x !== p) : [...phases, p].sort();
      q.delete('phase');
      cur.forEach((x) => q.append('phase', String(x)));
      if (cur.length === 3) q.delete('phase');
      if (!cur.length) q.append('phase', '0');
    });
  const setList = (key: string, values: string[]) =>
    setParams((q) => {
      q.delete(key);
      values.forEach((v) => q.append(key, v));
    });

  const query: PageQuery = $derived({ namespace: scope.current, phases, tags: tagSel, entities: entitySel, sort, desc, ids: agentSel.length ? (agentIds.length ? agentIds : ['00000000-0000-0000-0000-000000000000']) : [] });
  const key = $derived(JSON.stringify(query));

  // ── Rows: fetched in pages as the virtual window moves ────────────────
  const first = createQuery(() => ({
    queryKey: ['memories', key, 0],
    queryFn: () => memoryPage(query, 0),
    placeholderData: (prev) => prev,
  }));
  const total = $derived(first.data?.total ?? 0);
  let pages = $state<Record<string, MemoryRow[]>>({});
  let loading = new Set<string>();
  $effect(() => {
    key;
    pages = {};
    loading = new Set();
  });
  $effect(() => {
    if (first.data) pages[`${key}:0`] = first.data.rows;
  });
  function rowAt(i: number): MemoryRow | undefined {
    const p = Math.floor(i / PAGE);
    const rows = pages[`${key}:${p}`];
    if (rows) return rows[i - p * PAGE];
    const k = `${key}:${p}`;
    if (!loading.has(k) && p > 0) {
      loading.add(k);
      const forKey = key;
      memoryPage(query, p * PAGE).then((res) => {
        if (forKey === key) pages[k] = res.rows;
      });
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
  const toggle = (id: string) => selection.select(selection.id === id ? null : id);
  const padTop = $derived(items.length ? items[0].start : 0);
  const padBottom = $derived(items.length ? $virtualizer.getTotalSize() - items[items.length - 1].end : 0);

  // ── Filters' options ──────────────────────────────────────────────────
  const tagList = createQuery(() => ({ queryKey: ['tags', scope.current], queryFn: () => fetchTags(scope.current) }));

  // ── Selection and keyboard ────────────────────────────────────────────
  const selectedIndex = $derived.by(() => {
    if (!selection.id) return -1;
    for (const [k, rows] of Object.entries(pages)) {
      if (!k.startsWith(key)) continue;
      const i = rows.findIndex((r) => r.id === selection.id);
      if (i >= 0) return Number(k.slice(key.length + 1)) * PAGE + i;
    }
    return -1;
  });
  // A memory selected from elsewhere (search) scrolls into view and opens.
  $effect(() => {
    const i = selectedIndex;
    if (i >= 0) untrack(() => $virtualizer.scrollToIndex(i, { align: 'auto' }));
  });

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape' && selection.id) {
      selection.select(null);
      return;
    }
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const target = e.target as HTMLElement;
    if (target.tagName === 'INPUT') return;
    e.preventDefault();
    const next = Math.min(total - 1, Math.max(0, selectedIndex + (e.key === 'ArrowDown' ? 1 : -1)));
    const row = rowAt(next);
    if (row) {
      selection.select(row.id);
      $virtualizer.scrollToIndex(next, { align: 'auto' });
      requestAnimationFrame(() => scroller?.querySelector<HTMLButtonElement>(`[data-id="${row.id}"]`)?.focus());
    }
  }

  const now = Date.now();
  function next(m: MemoryRow) {
    if (!m.nextChange) return m.permastore ? 'Permastore' : '—';
    const days = (m.nextChange.at - now) / DAY_MS;
    return `${m.nextChange.to === 4 ? 'Deleted' : phaseName(m.nextChange.to)} ${days <= 0 ? 'next sweep' : `in ${duration(days)}`}`;
  }
</script>

<svelte:window {onkeydown} />

<Screen>
  <PageHeader title="Explore" meta={first.data ? `${count(total)} memories` : undefined} />

  <div role="toolbar" aria-label="Filters" class="toolbar">
    <ScopeChip />
    <span class="toolbar-divider"></span>
    {#each [1, 2, 3] as p (p)}
      <button type="button" class="chip" aria-pressed={phases.includes(p)} onclick={() => togglePhase(p)}>{phaseName(p)}</button>
    {/each}
    <span class="toolbar-divider"></span>
    <FilterMenu label="Tags" options={(tagList.data?.tags.items ?? []).map((t: { name: string; count: number }) => ({ value: t.name, count: t.count }))}
      bind:selected={() => tagSel, (v) => setList('tag', v)} />
    <FilterMenu label="Entities" options={(tagList.data?.entities.items ?? []).map((t: { name: string; count: number }) => ({ value: t.name, count: t.count }))}
      bind:selected={() => entitySel, (v) => setList('entity', v)} />
    <FilterMenu label="Agent" options={live.agents.map((a) => ({ value: a }))} empty="Agents appear here once they act while the UI is open"
      bind:selected={() => agentSel, (v) => setList('agent', v)} />
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

  {#if selection.id && selectedIndex < 0 && !first.isPending}
    <!-- Selected from search but not among the loaded rows: show it above the table. -->
    <div class="pinned">
      <MemoryPanel id={selection.id} layout="inline" heading onclose={() => selection.select(null)} onopen={(id) => selection.select(id)}
        secondary={{ label: 'Show on map', onclick: () => goto(`/map?id=${selection.id}`) }} />
    </div>
  {/if}

  <div class="table-card" bind:this={scroller}>
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
            <tr class="row"><td colspan="6" class="skel-cell"><span class="skeleton bar"></span></td></tr>
          {/each}
        </tbody>
      {:else if total === 0}
        <tbody><tr><td colspan="6" class="empty">{phases.length === 3 && !tagSel.length && !entitySel.length ? 'No memories in this scope yet. Agents add them with store_memory.' : 'Nothing matches these filters.'}</td></tr></tbody>
      {:else}
        {#if padTop > 0}<tbody aria-hidden="true"><tr style:height="{padTop}px"></tr></tbody>{/if}
        {#each items as item (item.key)}
          {@const m = rowAt(item.index)}
          {@const open = !!m && m.id === selection.id}
          <!-- One tbody per row so the virtualizer can measure an expanded row. -->
          <tbody data-index={item.index} use:measure>
            <!-- The whole row is a click target; the summary button is the keyboard path. -->
            <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
            <tr class="row" class:selected={open} onclick={() => m && toggle(m.id)}>
              {#if m}
                {@const r = strengthAt(m.lastAccessedAt, m.stability, m.decayMultiplier)}
                <td class="first cell-btn">
                  <button type="button" data-id={m.id} aria-expanded={open} class:ghost={m.summary === null} onclick={(e) => { e.stopPropagation(); toggle(m.id); }}>{m.summary ?? 'Summary dropped'}</button>
                </td>
                <td><span class="phase"><span class="ph ph-{m.phase}"></span>{phaseName(m.phase)}</span></td>
                <td><span class="strength"><span class="track"><span class="fill" style:width="{Math.round(r * 100)}%"></span></span><span class="mono num">{r.toFixed(2)}</span></span></td>
                <td class="meta">{next(m)}</td>
                <td class="mono num muted">{m.namespace}</td>
                <td class="mono num muted last">{ago(m.lastAccessedAt)}</td>
              {:else}
                <td colspan="6" class="skel-cell"><span class="skeleton bar"></span></td>
              {/if}
            </tr>
            {#if open && m}
              <tr class="expanded">
                <td colspan="6">
                  <MemoryPanel id={m.id} layout="inline" onclose={() => selection.select(null)} onopen={(id) => selection.select(id)}
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
  .table-card {
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
  .row {
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
  .row.selected {
    background: var(--color-accent-soft);
  }
  td {
    padding: 0 var(--space-3);
    white-space: nowrap;
  }
  td.cell-btn {
    padding: 0;
    white-space: normal;
    max-width: 0;
    width: 100%;
  }
  .cell-btn button {
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
  .row.selected .cell-btn button {
    white-space: normal;
  }
  .cell-btn button.ghost {
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
  .ph-2 {
    opacity: 0.38;
  }
  .ph-3 {
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
  .empty {
    padding: var(--space-8) var(--space-4);
    color: var(--color-text-muted);
    white-space: normal;
  }
</style>
