<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A whole-screen wait inside the shell: mark, serif title, one or two sentences,
     an optional progress bar, a mono detail line and an optional action. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import Logo from './Logo.svelte';
  let {
    title,
    breathing = false,
    bar = null,
    detail = undefined,
    children,
    action,
  }: { title: string; breathing?: boolean; bar?: 'indeterminate' | null; detail?: string; children?: Snippet; action?: Snippet } = $props();
</script>

<div class="wait" class:breathing role="status">
  <Logo variant="mark" />
  <h2>{title}</h2>
  {#if children}<p>{@render children()}</p>{/if}
  {#if bar}<div class="progress {bar}"><span></span></div>{/if}
  {#if detail}<span class="detail">{detail}</span>{/if}
  {#if action}<div class="actions">{@render action()}</div>{/if}
</div>
