<!-- The main column every view shares: padding, gap, daemon banner. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  import DaemonBanner from './DaemonBanner.svelte';
  let { children, gap = 'var(--space-4)' }: { children: Snippet; gap?: string } = $props();
</script>

<main class="screen" style:gap={gap}>
  <DaemonBanner />
  {@render children()}
</main>

<style>
  .screen {
    flex: 999 1 var(--main-min);
    min-width: 0;
    min-height: 0;
    height: 100%;
    display: flex;
    flex-direction: column;
    padding: var(--space-7) var(--space-8);
    overflow: auto;
  }
  @media (max-width: 600px) {
    .screen {
      padding: var(--space-5) var(--space-4);
      height: auto;
    }
  }
</style>
