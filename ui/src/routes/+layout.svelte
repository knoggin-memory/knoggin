<script lang="ts">
  import '../app.css';
  import { QueryClient, QueryClientProvider } from '@tanstack/svelte-query';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import Palette from '$lib/components/Palette.svelte';
  import { session } from '$lib/session.svelte';
  import { palette, isPaletteShortcut } from '$lib/palette.svelte';

  let { children } = $props();
  const client = new QueryClient({
    defaultOptions: { queries: { retry: 1, refetchOnWindowFocus: false, staleTime: 5_000 } },
  });

  $effect(() => {
    session.refresh();
    const t = setInterval(() => session.refresh(), 10_000);
    return () => clearInterval(t);
  });

  function onkeydown(e: KeyboardEvent) {
    if (isPaletteShortcut(e)) {
      e.preventDefault();
      palette.show();
    } else if (e.key === 'Escape' && palette.open) {
      palette.hide();
    }
  }
</script>

<svelte:window {onkeydown} />

<QueryClientProvider {client}>
  <div class="shell">
    <Sidebar />
    {@render children()}
  </div>
  <Palette />
</QueryClientProvider>

<style>
  .shell {
    height: 100vh;
    display: flex;
    background: var(--color-bg);
    color: var(--color-text);
  }
  @media (max-width: 600px) {
    .shell {
      flex-direction: column;
      height: auto;
      min-height: 100vh;
    }
  }
</style>
