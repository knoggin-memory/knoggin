<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A standalone centred column for the sign-in pages, the local "Open Knoggin from your
     terminal" page and the whole-page states. Fluid, no breakpoints. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import Logo from './Logo.svelte';
  let {
    title,
    pageTitle = undefined,
    stacked = false,
    hideTitle = false,
    breathing = false,
    busy = false,
    detail = undefined,
    sourceUrl = undefined,
    foot = undefined,
    wide = false,
    children,
    actions,
  }: {
    title: string;
    pageTitle?: string;
    stacked?: boolean;
    hideTitle?: boolean;
    breathing?: boolean;
    busy?: boolean;
    detail?: string;
    /** The "Source code" link the AGPL asks for. */
    sourceUrl?: string;
    foot?: string;
    wide?: boolean;
    children?: Snippet;
    actions?: Snippet;
  } = $props();
</script>

<svelte:head><title>Knoggin · {pageTitle ?? title}</title></svelte:head>

<div class="standalone">
  <main class="auth" class:breathing>
    <div class="auth-col" class:wide>
      <Logo variant={stacked ? 'stacked' : 'mark'} alt="Knoggin" />
      <h1 class:visually-hidden={hideTitle}>{title}</h1>
      {@render children?.()}
      {#if busy}<div class="progress indeterminate" role="progressbar" aria-label="Working"><span></span></div>{/if}
      {#if detail}<span class="detail">{detail}</span>{/if}
      {#if actions}<div class="actions">{@render actions()}</div>{/if}
    </div>
  </main>
  {#if foot || sourceUrl}
    <footer class="auth-foot">
      {#if foot}<span>{foot}</span>{/if}
      {#if sourceUrl}<a href={sourceUrl}>Source code</a>{/if}
    </footer>
  {/if}
</div>
