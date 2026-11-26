<!-- ⌘K palette (DESIGN §5, §8 Search): recalld's own hybrid search, read-only,
     with explain. No UI-side index: every keystroke asks the daemon. -->
<script lang="ts">
  import { goto } from '$app/navigation';
  import { page } from '$app/state';
  import { palette } from '$lib/palette.svelte';
  import { scope } from '$lib/scope.svelte';
  import { selection } from '$lib/selection.svelte';
  import { search } from '$lib/api/memories';
  import type { UiSearch, UiSearchHit } from '$lib/api/types';
  import { truncateWords } from '$lib/format';
  import Icon from './Icon.svelte';
  import ScopeChip from './ScopeChip.svelte';

  const EDGE: Record<number, string> = { 1: 'parent', 2: 'associative', 3: 'causal', 4: 'contradicts', 5: 'entity', 6: 'temporal', 7: 'supersedes' };
  const mac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform);

  let input: HTMLInputElement | undefined = $state();
  let q = $state('');
  let result = $state<UiSearch | null>(null);
  let pending = $state(false);
  let error = $state<string | null>(null);
  let active = $state(0);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let seq = 0;

  // The search filter starts from the page's namespace scope (All when none is
  // set) each time the palette opens; changing it here doesn't change the page.
  let searchScope = $state<string | null>(null);
  $effect(() => {
    if (palette.open) {
      searchScope = scope.current;
      queueMicrotask(() => input?.select());
    }
  });

  // Debounce 120 ms; keep showing the last results while the next query runs.
  $effect(() => {
    const text = q.trim();
    const ns = searchScope;
    clearTimeout(timer);
    if (!text) {
      result = null;
      pending = false;
      return;
    }
    timer = setTimeout(async () => {
      const mine = ++seq;
      pending = true;
      try {
        const r = await search(text, ns, 10);
        if (mine === seq) {
          result = r;
          error = null;
          active = 0;
          requestAnimationFrame(() => performance.mark('recalld:search-results'));
        }
      } catch (e) {
        if (mine === seq) error = (e as Error).message;
      } finally {
        if (mine === seq) pending = false;
      }
    }, 120);
  });

  const direct = $derived((result?.results ?? []).filter((h) => h.activation === null));
  const linked = $derived((result?.results ?? []).filter((h) => h.activation !== null));
  const ordered = $derived([...direct, ...linked]);
  const groups = $derived(
    result && !result.merged
      ? [...new Set(direct.map((h) => h.namespace))].map((ns) => ({ title: ns, hits: direct.filter((h) => h.namespace === ns) }))
      : [{ title: 'Best matches', hits: direct }],
  );

  function explain(h: UiSearchHit): string {
    if (h.activation !== null) {
      const edge = h.viaEdgeType !== null ? `over ${EDGE[h.viaEdgeType] === 'associative' || EDGE[h.viaEdgeType] === 'entity' ? 'an' : 'a'} ${EDGE[h.viaEdgeType] ?? ''} link ` : '';
      return `reached in ${h.hops ?? 1} hop${(h.hops ?? 1) > 1 ? 's' : ''} ${edge}· activation ${h.activation.toFixed(2)}`;
    }
    const parts: string[] = [];
    if (h.vector !== null) parts.push(`vector ${h.vector.toFixed(2)}`);
    // The pipeline's text and entity boosts, as their contribution to the score.
    const boost = h.vector !== null ? h.score - h.vector : 0;
    if (boost > 0.005) parts.push(`text and entities +${boost.toFixed(2)}`);
    else if (h.vector === null && h.text !== null) parts.push('keyword match');
    if (h.contradicts) parts.push(`contradicts “${truncateWords(h.contradicts, 20)}”`);
    if (!result?.merged || searchScope === null) parts.push(`${h.namespace} namespace`);
    return parts.join(' · ');
  }

  /** Open a result where you are: on the map it is selected and centered
   *  there, on Explore it opens in Explore; elsewhere (or with ⌘↵ / Ctrl+click)
   *  it goes to the map. */
  function open(h: UiSearchHit, toMap: boolean) {
    selection.select(h.id);
    palette.hide();
    const here = page.url.pathname;
    if (here.startsWith('/explore') && !toMap) return;
    if (here.startsWith('/map')) goto(`/map?id=${h.id}`, { replace: true });
    else goto(toMap ? `/map?id=${h.id}` : `/explore?id=${h.id}`);
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      palette.hide();
    } else if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (ordered.length) active = (active + (e.key === 'ArrowDown' ? 1 : ordered.length - 1)) % ordered.length;
    } else if (e.key === 'Enter' && ordered[active]) {
      e.preventDefault();
      open(ordered[active], e.metaKey || e.ctrlKey);
    }
  }
</script>

{#snippet row(h: UiSearchHit, i: number, isLinked: boolean)}
  <button type="button" role="option" id="hit-{i}" aria-selected={i === active} class="hit" class:on={i === active}
    onmouseenter={() => (active = i)} onclick={(e) => open(h, e.metaKey || e.ctrlKey)}>
    <span class="hit-line">
      <span class="summary" class:ghost={h.summary === null}>{h.summary ?? 'Summary dropped'}</span>
      {#if isLinked}
        <span class="via">via {truncateWords(h.viaSummary ?? '…', 18)}</span>
      {:else}
        <span class="mono tag">{h.namespace}</span>
        {#if h.contradicts}<span class="contra">contradicted</span>{/if}
      {/if}
      <span class="mono score"><span class="track"><span class="fill" class:moss={isLinked} style:width="{Math.round(Math.min(1, h.score) * 100)}%"></span></span>{h.score.toFixed(2)}</span>
    </span>
    {#if i === active}<span class="mono why">{explain(h)}</span>{/if}
  </button>
{/snippet}

{#if palette.open}
  <div class="scrim" role="presentation" onclick={() => palette.hide()}></div>
  <div class="palette" role="dialog" aria-modal="true" aria-label="Search memories">
    <div class="input-row">
      <Icon name="search" stroke={1.8} />
      <label for="palette-q" class="visually-hidden">Search memories</label>
      <input id="palette-q" type="search" bind:this={input} bind:value={q} {onkeydown} placeholder="Search memories"
        autocomplete="off" role="combobox" aria-expanded={ordered.length > 0} aria-controls="palette-results"
        aria-activedescendant={ordered.length ? `hit-${active}` : undefined} />
      <button type="button" class="esc" aria-label="Close search" onclick={() => palette.hide()}><kbd>Esc</kbd></button>
    </div>
    <div class="scope-row">
      <ScopeChip value={searchScope} onchange={(ns) => (searchScope = ns)} />
      <span class="note" title="Search here uses recordAccess:false, so results are not reinforced">Browsing won’t reinforce memories</span>
    </div>

    <div id="palette-results" role="listbox" aria-label="Results" class="results" aria-busy={pending}>
      {#if error}
        <p class="state">{error}</p>
      {:else if !q.trim()}
        <p class="state">Search recalld’s memories with its hybrid search: vector, text, entities and links.</p>
      {:else if result && !ordered.length && !pending}
        <p class="state">No memories match “{q.trim()}”.</p>
      {:else if !result}
        {#each [0, 1, 2] as i (i)}<span class="skeleton skel"></span>{/each}
      {/if}
      {#each groups as g (g.title)}
        {#if g.hits.length}
          <span class="micro-label group">{g.title}</span>
          {#each g.hits as h (h.id)}{@render row(h, ordered.indexOf(h), false)}{/each}
        {/if}
      {/each}
      {#if linked.length}
        <span class="micro-label group">Through links</span>
        {#each linked as h (h.id)}{@render row(h, ordered.indexOf(h), true)}{/each}
      {/if}
    </div>

    <div class="footer">
      <span class="hint"><kbd>↑↓</kbd>move</span>
      <span class="hint"><kbd>↵</kbd>open</span>
      <span class="hint"><kbd>{mac ? '⌘↵' : 'Ctrl ↵'}</kbd>show on map</span>
      <span class="mono count">{result ? `${ordered.length} results · ` : ''}recalld hybrid search</span>
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    background: var(--color-scrim);
    z-index: 40;
    animation: fade var(--dur-palette) var(--ease-out);
  }
  .palette {
    position: fixed;
    top: var(--palette-top);
    left: 50%;
    transform: translateX(-50%);
    width: min(var(--palette-w), calc(100vw - var(--space-6) * 2));
    max-height: calc(100vh - var(--palette-top) - var(--space-6));
    display: flex;
    flex-direction: column;
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    z-index: 41;
    overflow: hidden;
    animation: rise var(--dur-palette) var(--ease-out);
  }
  @keyframes fade {
    from {
      opacity: 0;
    }
  }
  @keyframes rise {
    from {
      opacity: 0;
      transform: translate(-50%, var(--space-1));
    }
  }
  .input-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 0 calc(var(--space-4) + var(--space-1) / 2);
    border-bottom: var(--hairline) solid var(--color-border);
    color: var(--color-text-muted);
  }
  input {
    flex: 1 1 auto;
    min-width: 0;
    height: var(--palette-row);
    border: 0;
    outline: none;
    background: transparent;
    color: var(--color-text);
    font: var(--text-input-lg) var(--font-sans);
  }
  input::-webkit-search-cancel-button {
    display: none;
  }
  .esc {
    border: 0;
    background: none;
    padding: 0;
    cursor: pointer;
  }
  .scope-row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: var(--space-2);
    padding: calc(var(--space-2) + var(--space-1) / 2) calc(var(--space-4) + var(--space-1) / 2);
    border-bottom: var(--hairline) solid var(--color-divider);
  }
  .note {
    margin-left: auto;
    font-size: var(--text-small);
    color: var(--color-text-muted);
  }
  .results {
    display: flex;
    flex-direction: column;
    padding: var(--space-2) var(--space-2) calc(var(--space-2) + var(--space-1) / 2);
    overflow-y: auto;
    min-height: 0;
  }
  .group {
    padding: var(--space-2) calc(var(--space-2) + var(--space-1) / 2) var(--space-1);
  }
  .group ~ .group {
    padding-top: var(--space-3);
  }
  .state {
    margin: 0;
    padding: var(--space-4) calc(var(--space-2) + var(--space-1) / 2);
    color: var(--color-text-muted);
  }
  .skel {
    height: var(--space-6);
    margin: var(--space-2) calc(var(--space-2) + var(--space-1) / 2);
  }
  .hit {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    width: 100%;
    padding: calc(var(--space-2) + var(--space-1) / 2);
    border: 0;
    border-radius: var(--radius-md);
    background: transparent;
    text-align: left;
    color: var(--color-text);
    cursor: pointer;
  }
  .hit.on {
    background: var(--color-accent-soft);
  }
  .hit-line {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    width: 100%;
  }
  .summary {
    flex: 1 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .summary.ghost {
    font-style: italic;
    color: var(--color-text-muted);
  }
  .tag {
    flex: 0 0 auto;
    font-size: var(--text-status);
    color: var(--color-text-muted);
  }
  .via {
    flex: 0 0 auto;
    font-size: var(--text-small);
    color: var(--color-text-muted);
  }
  .contra {
    flex: 0 0 auto;
    padding: var(--hairline) calc(var(--space-2) - var(--hairline));
    border-radius: var(--radius-sm);
    border: var(--hairline) dashed var(--color-contradicts);
    color: var(--color-suppressed-text);
    font-size: var(--text-status);
  }
  .score {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-value);
    color: var(--color-text-2);
  }
  .track {
    width: var(--score-bar-w);
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
  .fill.moss {
    background: var(--color-agent-2);
  }
  .why {
    font-size: var(--text-explain);
    color: var(--color-text-muted);
  }
  .footer {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: calc(var(--space-4) + var(--space-1) / 2);
    padding: calc(var(--space-2) + var(--space-1) / 2) calc(var(--space-4) + var(--space-1) / 2);
    border-top: var(--hairline) solid var(--color-divider);
    background: var(--color-sunken);
    font-size: var(--text-small);
    color: var(--color-text-muted);
  }
  .hint {
    display: flex;
    align-items: center;
    gap: calc(var(--space-1) * 1.5);
  }
  .hint kbd {
    background: var(--color-surface);
  }
  .count {
    margin-left: auto;
  }
</style>
