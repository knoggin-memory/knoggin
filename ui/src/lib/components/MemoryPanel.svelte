<!-- Detail for the selected memory (DESIGN §2, §8): a side panel on the map,
     an expanded row in Explore (`layout="inline"`, which shows more: full
     text, every label, links and a longer activity list). Reads are
     read-only; Reinforce and Forget are the only writes, Forget confirms. -->
<script lang="ts">
  import { createQuery, useQueryClient } from '@tanstack/svelte-query';
  import { AlertDialog, DropdownMenu } from 'bits-ui';
  import Icon from './Icon.svelte';
  import DecayCurve from './DecayCurve.svelte';
  import { forget, memoryHistory, memoryNeighbors, memoryRecord, memoryRows, reinforce } from '$lib/api/memories';
  import { EDGE_LABELS } from '$lib/map/encoding';
  import { ago, duration, phaseName } from '$lib/format';
  import { DAY_MS, strengthAt } from '$lib/fsrs';

  let {
    id,
    onclose,
    secondary,
    layout = 'panel',
    heading = false,
    onopen,
  }: {
    id: string;
    onclose: () => void;
    /** "Show on map" in Explore, "Local graph" on the map. */
    secondary: { label: string; onclick: () => void };
    layout?: 'panel' | 'inline';
    /** Inline only: show the summary as a title, for when no row above shows it. */
    heading?: boolean;
    /** Inline only: open a linked memory. */
    onopen?: (id: string) => void;
  } = $props();
  const inline = $derived(layout === 'inline');

  const client = useQueryClient();
  const row = createQuery(() => ({
    queryKey: ['memory-row', id],
    queryFn: async () => (await memoryRows([id]))[0] ?? null,
  }));
  const record = createQuery(() => ({ queryKey: ['memory', id], queryFn: () => memoryRecord(id) }));
  const history = createQuery(() => ({ queryKey: ['history', id, inline], queryFn: () => memoryHistory(id, inline ? 12 : 3) }));
  // Direct links, strongest first (inline only).
  const neighbors = createQuery(() => ({
    queryKey: ['neighbors', id, 1],
    queryFn: () => memoryNeighbors(id, 1),
    enabled: inline,
  }));
  const links = $derived.by(() => {
    const n = neighbors.data;
    if (!n) return [];
    const byId = new Map(n.nodes.map((x) => [x.id, x]));
    return n.edges
      .filter((e) => e.src === id || e.dst === id)
      .map((e) => ({ ...e, other: byId.get(e.src === id ? e.dst : e.src) }))
      .filter((e) => e.other)
      .sort((a, b) => b.weight - a.weight);
  });
  const fmtDate = (t: number) => new Date(t).toLocaleDateString('en-US', { month: 'short', day: 'numeric', year: 'numeric' });

  let showFull = $state(false);
  // Summaries longer than this show smaller and clamp to four lines.
  const LONG_TITLE = 90;
  let showTitle = $state(false);
  $effect(() => {
    id;
    showTitle = false;
  });
  let previous = $state<number | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let confirming = $state(false);

  const r = $derived(row.data ? strengthAt(row.data.lastAccessedAt, row.data.stability, row.data.decayMultiplier) : 0);
  const elapsed = $derived(row.data ? Math.max(0, Date.now() - row.data.lastAccessedAt) / DAY_MS : 0);
  const labels = $derived(
    [...(row.data?.entities ?? []), ...(row.data?.tags ?? [])].slice(0, 3).join(' · '),
  );
  const KINDS: Record<string, string> = {
    direct: 'found it searching',
    associative: 'reached it through a link',
    reinforce: 'reinforced it',
    sweep: 'Decay sweep checked it',
  };
  type Act = { at: number; kind: string; agent: string | null; query: string | null; n: number };
  // Consecutive identical entries (same agent, kind and query) collapse into one row.
  const acts = $derived(
    (history.data?.events ?? []).reduce<Act[]>((out, e) => {
      const last = out.at(-1);
      if (last && last.kind === e.kind && last.agent === e.agent && last.query === e.query) last.n++;
      else out.push({ at: e.at, kind: e.kind, agent: e.agent, query: e.query, n: 1 });
      return out;
    }, []),
  );

  async function doReinforce(rating: number) {
    if (!row.data) return;
    busy = true;
    error = null;
    previous = row.data.stability;
    try {
      await reinforce(id, row.data.namespace, rating);
      await client.invalidateQueries();
    } catch (e) {
      error = (e as Error).message;
      previous = null;
    } finally {
      busy = false;
    }
  }

  async function doForget() {
    if (!row.data) return;
    busy = true;
    try {
      await forget(id, row.data.namespace);
      confirming = false;
      await client.invalidateQueries();
      onclose();
    } catch (e) {
      error = (e as Error).message;
    } finally {
      busy = false;
    }
  }
</script>

{#snippet curve(m: NonNullable<typeof row.data>)}
  <figure>
    <div class="fig-head">
      <span class="card-title">Retrievability</span>
      <span class="mono values">R {r.toFixed(2)} · S {duration(m.stability)}{m.permastore ? ' · permastore' : ''}</span>
    </div>
    <DecayCurve stability={m.stability} elapsedDays={elapsed} previousStability={previous} multiplier={m.decayMultiplier} />
    <figcaption>
      {m.nextChange ? `${phaseName(m.nextChange.to)} ${m.nextChange.at <= Date.now() ? 'at the next sweep' : `in ${duration((m.nextChange.at - Date.now()) / DAY_MS)}`}` : m.permastore ? 'In permastore' : 'No change expected'}
      · created {fmtDate(m.createdAt)}
    </figcaption>
  </figure>
{/snippet}

{#snippet activity()}
  <section class="activity" aria-label="Recent activity">
    <span class="micro-label">Activity</span>
    {#each acts as e, i (i)}
      <div class="act">
        <span class="mono t">{ago(e.at)}</span>
        <span>
          {#if e.kind !== 'sweep'}{e.agent ?? 'An agent'}{/if}
          <span class="muted">{KINDS[e.kind] ?? e.kind}{e.n > 1 ? ` · ${e.n}×` : ''}</span>
          {#if e.query}<span class="q">“{e.query}”</span>{/if}
        </span>
      </div>
    {:else}
      <span class="muted small">No access recorded yet.</span>
    {/each}
  </section>
{/snippet}

{#snippet actions(m: NonNullable<typeof row.data>)}
  {#if error}<p class="error" role="alert">{error}</p>{/if}
  <div class="actions">
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class="btn-primary grow" disabled={busy}>Reinforce</DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class="menu" sideOffset={6} align="start">
          {#each [[4, 'Easy'], [3, 'Good'], [2, 'Hard'], [1, 'Again']] as [q, label] (q)}
            <DropdownMenu.Item class="menu-item" onSelect={() => doReinforce(q as number)}>{label}</DropdownMenu.Item>
          {/each}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
    <button type="button" class="btn-secondary grow" onclick={secondary.onclick}>{secondary.label}</button>
    <DropdownMenu.Root>
      <DropdownMenu.Trigger class="btn-secondary square" aria-label="More actions"><Icon name="more" /></DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content class="menu" sideOffset={6} align="end">
          <DropdownMenu.Item class="menu-item" onSelect={() => (confirming = true)}>Forget…</DropdownMenu.Item>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  </div>

  <AlertDialog.Root bind:open={confirming}>
    <AlertDialog.Portal>
      <AlertDialog.Overlay class="dialog-scrim" />
      <AlertDialog.Content class="dialog">
        <AlertDialog.Title class="card-title">Forget this memory?</AlertDialog.Title>
        <AlertDialog.Description class="dialog-text">
          “{m.summary ?? 'Summary dropped'}” becomes a tombstone: its content is removed and it no longer appears in recall.
        </AlertDialog.Description>
        <div class="dialog-actions">
          <AlertDialog.Cancel class="btn-secondary">Keep it</AlertDialog.Cancel>
          <AlertDialog.Action class="btn-primary" disabled={busy} onclick={doForget}>Forget</AlertDialog.Action>
        </div>
      </AlertDialog.Content>
    </AlertDialog.Portal>
  </AlertDialog.Root>
{/snippet}

{#if inline}
  <section class="detail" aria-label="Memory details">
    {#if row.isPending}
      <div class="skeleton block"></div>
    {:else if !row.data}
      <p class="muted">This memory no longer exists.</p>
    {:else}
      {@const m = row.data}
      {#if heading}
        <div class="inline-head">
          <h2 class:ghost={m.summary === null}>{m.summary ?? 'Summary dropped'}</h2>
          <button type="button" class="icon-btn" aria-label="Close details" onclick={onclose}><Icon name="close" stroke={1.8} /></button>
        </div>
      {/if}
      <div class="col">
        <span class="micro-label">Content</span>
        {#if record.data?.fullText}
          <p class="full">{record.data.fullText}</p>
        {:else}
          <p class="muted small">{m.phase === 1 ? 'No full text stored.' : 'Full text dropped when it decayed to a Summary.'}</p>
        {/if}
        {#if m.entities.length}
          <div class="labels"><span class="micro-label">Entities</span>{#each m.entities as e (e)}<span class="tag">{e}</span>{/each}</div>
        {/if}
        {#if m.tags.length}
          <div class="labels"><span class="micro-label">Tags</span>{#each m.tags as t (t)}<span class="tag">{t}</span>{/each}</div>
        {/if}
        <dl class="facts">
          <div><dt>Namespace</dt><dd class="mono">{m.namespace}</dd></div>
          <div><dt>Created</dt><dd class="mono">{fmtDate(m.createdAt)}</dd></div>
          <div><dt>Last accessed</dt><dd class="mono">{ago(m.lastAccessedAt)}</dd></div>
          <div><dt>Stability</dt><dd class="mono">{duration(m.stability)}</dd></div>
          <div><dt>ID</dt><dd class="mono id">{m.id}</dd></div>
        </dl>
      </div>
      <div class="col">
        {@render curve(m)}
        {@render activity()}
      </div>
      <div class="col">
        <span class="micro-label">Links · {m.degree}</span>
        {#if neighbors.isPending}
          <span class="skeleton line"></span>
        {:else}
          <ul class="links">
            {#each links.slice(0, 10) as l, i (i)}
              <li>
                <button type="button" class="link-row" onclick={() => onopen?.(l.other!.id)} title="Open this memory">
                  <span class="mono kind">{EDGE_LABELS[l.type] ?? l.type}</span><span class="other" class:ghost={l.other!.summary === null}>{l.other!.summary ?? 'Summary dropped'}</span><span class="mono w">{l.weight.toFixed(2)}</span>
                </button>
              </li>
            {:else}
              <li class="muted small">No links.</li>
            {/each}
          </ul>
          {#if links.length > 10}<span class="muted small">and {links.length - 10} more</span>{/if}
        {/if}
        {@render actions(m)}
      </div>
    {/if}
  </section>
{:else}
<aside class="panel" aria-label="Selected memory">
  {#if row.isPending}
    <div class="skeleton line"></div>
    <div class="skeleton title"></div>
    <div class="skeleton block"></div>
  {:else if !row.data}
    <p class="muted">This memory no longer exists.</p>
  {:else}
    {@const m = row.data}
    {@const long = (m.summary?.length ?? 0) > LONG_TITLE}
    <div class="head">
      <div class="top">
        <span class="status mono"><span class="dot"></span>{phaseName(m.phase)} · accessed {ago(m.lastAccessedAt)}</span>
        <button type="button" class="icon-btn close" aria-label="Close details" onclick={onclose}><Icon name="close" stroke={1.8} /></button>
      </div>
      <h2 class:ghost={m.summary === null} class:long class:clamped={long && !showTitle}>{m.summary ?? 'Summary dropped'}</h2>
      {#if long}
        <button type="button" class="link" aria-expanded={showTitle} onclick={() => (showTitle = !showTitle)}>
          {showTitle ? 'Show less' : 'Show whole summary'}
        </button>
      {/if}
      <span class="meta">{m.namespace}{labels ? ` · ${labels}` : ''} · {m.degree} links</span>
      {#if m.phase === 1 && record.data?.fullText}
        <button type="button" class="link" aria-expanded={showFull} onclick={() => (showFull = !showFull)}>
          {showFull ? 'Hide full text' : 'Show full text'}
        </button>
        {#if showFull}<p class="full">{record.data.fullText}</p>{/if}
      {/if}
    </div>

    {@render curve(m)}
    {@render activity()}
    {@render actions(m)}
  {/if}
</aside>
{/if}

<style>
  .panel {
    flex: 0 1 var(--panel-width);
    min-width: var(--panel-min);
    height: 100%;
    display: flex;
    flex-direction: column;
    gap: var(--space-7);
    padding: var(--space-6) var(--space-7) var(--space-7);
    background: var(--color-surface);
    border-left: var(--hairline) solid var(--color-border);
    overflow: auto;
  }
  .head {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .top {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }
  .close {
    margin-right: calc(var(--space-2) * -1);
  }
  .status {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-status);
    color: var(--color-accent-text);
  }
  .dot {
    width: var(--dot);
    height: var(--dot);
    border-radius: var(--radius-pill);
    background: var(--color-accent);
  }
  h2 {
    margin: 0;
    font: var(--weight-regular) var(--text-memory) / 1.2 var(--font-serif);
    letter-spacing: -0.01em;
    text-wrap: pretty;
  }
  /* Long summaries drop to the section size and clamp until expanded. */
  h2.long {
    font-size: var(--text-section);
    line-height: 1.35;
  }
  h2.clamped {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    -webkit-line-clamp: 4;
    line-clamp: 4;
    overflow: hidden;
  }
  h2.ghost {
    font-style: italic;
    color: var(--color-text-muted);
  }
  .meta,
  figcaption,
  .small {
    font-size: var(--text-control);
    color: var(--color-text-muted);
  }
  .link {
    align-self: flex-start;
    padding: 0;
    border: 0;
    background: none;
    color: var(--color-accent-text);
    font-size: var(--text-control);
    text-decoration: underline;
    text-underline-offset: 0.2em;
    cursor: pointer;
  }
  .full {
    margin: 0;
    white-space: pre-wrap;
  }
  figure {
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .fig-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
  }
  .values {
    font-size: var(--text-status);
    color: var(--color-text-muted);
  }
  .activity {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .act {
    display: flex;
    gap: var(--space-3);
    font-size: var(--text-control);
  }
  .q {
    display: block;
    color: var(--color-text-2);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .act > span:last-child {
    min-width: 0;
  }
  .t {
    flex: 0 0 auto;
    min-width: var(--space-8);
    font-size: var(--text-status);
    color: var(--color-text-muted);
  }
  .muted {
    color: var(--color-text-muted);
  }
  .error {
    margin: 0;
    font-size: var(--text-control);
    color: var(--color-text);
  }
  .actions {
    display: flex;
    gap: var(--space-2);
    margin-top: auto;
  }
  .actions :global(.grow) {
    flex: 1 1 auto;
  }
  .actions :global(.square) {
    width: var(--button-h);
    padding: 0;
    color: var(--color-text-2);
  }
  .detail {
    display: grid;
    grid-template-columns: minmax(0, 1.2fr) minmax(0, 1fr) minmax(0, 1fr);
    gap: var(--space-7);
    padding: var(--space-6) var(--space-5) var(--space-7);
    white-space: normal;
    cursor: default;
    /* Easy to read: body size, dark text, roomy lines. */
    font-size: var(--text-body);
    line-height: 1.55;
    color: var(--color-text);
  }
  .detail h2 {
    font-size: var(--text-section);
  }
  .detail .micro-label {
    color: var(--color-text-2);
  }
  .detail .act,
  .detail .small,
  .detail figcaption {
    font-size: var(--text-body);
  }
  .detail .t,
  .detail .values {
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .detail .muted {
    color: var(--color-text-2);
  }
  .inline-head {
    grid-column: 1 / -1;
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: var(--space-3);
  }
  .col {
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    min-width: 0;
  }
  .detail .full {
    max-height: calc(var(--space-8) * 7);
    overflow: auto;
    font-size: var(--text-input);
    line-height: 1.6;
    color: var(--color-text);
  }
  .labels {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
  }
  .tag {
    padding: 0 var(--space-2);
    border: var(--hairline) solid var(--color-border-strong);
    border-radius: var(--radius-pill);
    font-size: var(--text-control);
    color: var(--color-text);
  }
  .facts {
    margin: 0;
    display: grid;
    gap: var(--space-2);
  }
  .facts div {
    display: flex;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .facts dt {
    color: var(--color-text-2);
  }
  .facts dd {
    margin: 0;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .id {
    font-size: var(--text-value);
  }
  .links {
    margin: 0 calc(var(--space-2) * -1);
    padding: 0;
    list-style: none;
    display: flex;
    flex-direction: column;
  }
  .link-row {
    width: 100%;
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    padding: var(--space-1) var(--space-2);
    border: 0;
    border-radius: var(--radius-sm);
    background: none;
    font: inherit;
    color: var(--color-text);
    text-align: left;
    cursor: pointer;
  }
  .link-row:hover,
  .link-row:focus-visible {
    background: var(--color-surface);
  }
  .links .kind {
    flex: 0 0 auto;
    min-width: calc(var(--space-8) * 1.5);
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .links .other {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .links .other.ghost {
    font-style: italic;
    color: var(--color-text-muted);
  }
  .links .w {
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .detail .actions {
    margin-top: auto;
  }
  @media (max-width: 1100px) {
    .detail {
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    }
  }
  @media (max-width: 700px) {
    .detail {
      grid-template-columns: minmax(0, 1fr);
    }
  }
  .skeleton.line {
    height: var(--space-3);
    width: 60%;
  }
  .skeleton.title {
    height: var(--space-8);
  }
  .skeleton.block {
    height: var(--space-8);
    flex: 0 0 calc(var(--space-8) * 3);
  }
  :global(.dialog-scrim) {
    position: fixed;
    inset: 0;
    background: var(--color-scrim);
    z-index: 50;
  }
  :global(.dialog) {
    position: fixed;
    top: var(--palette-top);
    left: 50%;
    transform: translateX(-50%);
    width: min(calc(var(--panel-width) + var(--space-8) * 2), calc(100vw - var(--space-8)));
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--card-pad);
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    z-index: 51;
  }
  :global(.dialog-text) {
    margin: 0;
    color: var(--color-text-2);
  }
  :global(.dialog-actions) {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  @media (max-width: 900px) {
    .panel {
      flex-basis: var(--panel-min);
    }
  }
  @media (max-width: 600px) {
    .panel {
      min-width: 0;
      height: auto;
      border-left: 0;
      border-top: var(--hairline) solid var(--color-border);
    }
  }
</style>
