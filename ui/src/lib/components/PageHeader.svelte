<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import { palette } from '$lib/palette.svelte';
  import Icon from './Icon.svelte';
  let {
    title,
    meta,
    crumb,
    pageTitle,
    children,
  }: { title: string; meta?: string; crumb?: { href: string; label: string }; pageTitle?: string; children?: Snippet } = $props();
  const mac = typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform);
</script>

<svelte:head><title>Knoggin · {pageTitle ?? title}</title></svelte:head>

<header class="header">
  <div class="title">
    {#if crumb}<a class="crumb" href={crumb.href}>{crumb.label}</a>{/if}
    <h1>{title}</h1>
    {#if meta}<span class="mono meta">{meta}</span>{/if}
    {@render children?.()}
  </div>
  <button
    type="button"
    class="search"
    aria-label="Search ({mac ? '⌘K' : 'Ctrl+K'})"
    aria-keyshortcuts="Meta+K Control+K /"
    onclick={() => palette.show()}
  >
    <Icon name="search" stroke={1.8} /><span>Search</span><kbd>{mac ? '⌘K' : 'Ctrl K'}</kbd>
  </button>
</header>

<style>
  .header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  .title {
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
  }
  h1 {
    margin: 0;
    font: var(--weight-regular) var(--text-page) / 1.15 var(--font-serif);
    letter-spacing: -0.01em;
  }
  .meta {
    font-size: var(--text-value);
    color: var(--color-text-muted);
  }
  .search {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    height: var(--button-h);
    padding: 0 var(--space-2) 0 var(--space-3);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-pill);
    background: var(--color-surface);
    color: var(--color-text-2);
    font: var(--text-control) var(--font-sans);
    cursor: pointer;
    text-align: left;
  }
  .search:hover {
    border-color: var(--color-border-strong);
  }
</style>
