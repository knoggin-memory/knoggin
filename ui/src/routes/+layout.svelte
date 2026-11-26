<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The shell, and the choice between it and the standalone pages. It first
     posts the one-time code of `knoggin open` (`/?t=<code>`) and reads the session. -->
<script lang="ts">
  import '../app.css';
  import { QueryClientProvider } from '@tanstack/svelte-query';
  import { goto, preloadCode, replaceState } from '$app/navigation';
  import { page } from '$app/state';
  import Sidebar from '$lib/components/Sidebar.svelte';
  import Palette from '$lib/components/Palette.svelte';
  import Standalone, { type Page } from '$lib/components/Standalone.svelte';
  import { session } from '$lib/session.svelte';
  import { store } from '$lib/store.svelte';
  import { status } from '$lib/status.svelte';
  import { live } from '$lib/live.svelte';
  import { prefs } from '$lib/prefs.svelte';
  import { palette, isPaletteShortcut } from '$lib/palette.svelte';
  import { queryClient } from '$lib/query';

  let { children } = $props();

  $effect(() => {
    prefs.apply();
    return prefs.watchSystem();
  });

  let started = false;
  $effect(() => {
    if (started) return;
    started = true;
    void (async () => {
      const code = new URL(location.href).searchParams.get('t');
      if (code) {
        await session.exchange(code);
        // The code works once; take it out of the address bar.
        const url = new URL(location.href);
        url.searchParams.delete('t');
        replaceState(url.pathname + url.search + url.hash, {});
      }
      status.start();
      await session.load();
    })();
  });

  // Load every screen's code once the shell is up: a local page's address dies with its
  // daemon, and a screen fetched later could not load to say "Knoggin is not running".
  let preloaded = false;
  $effect(() => {
    if (preloaded || session.phase !== 'ready' || !session.value?.signedIn) return;
    preloaded = true;
    const preload = preloadCode as (path: string) => Promise<void>;
    for (const p of ['/map', '/explore', '/timeline', '/health', '/keys', '/apps', '/apps/_', '/setup', '/account', '/users', '/status']) void preload(p);
    void import('$lib/map/MemoryMap.svelte');
  });

  // A store switch: read its status and follow its events.
  $effect(() => {
    const id = store.id;
    if (session.phase !== 'ready' || !session.value?.signedIn) return;
    live.ensureStore(id);
    void status.poll();
  });

  const here = $derived(page.url.pathname + page.url.search);
  const params = $derived(page.url.searchParams);

  const denied = () => {
    const s = session.value;
    return session.deniedReason ?? (s?.signedOutReason === 'account_removed' || s?.signedOutReason === 'not_allowed' ? s.signedOutReason : null);
  };

  /** Which standalone page shows, or null for the shell. */
  const standalone = $derived.by((): Page | 'redirect' | null => {
    const s = session.value;
    if (session.phase === 'locked') return { kind: 'locked' };
    if (session.phase === 'loading' || (session.phase === 'unreachable' && !s)) return session.phase === 'loading' ? { kind: 'loading' } : { kind: 'failed' };
    if (!s) return { kind: 'loading' };
    const onSignin = page.url.pathname === '/signin';
    const error = params.get('error');
    if (onSignin && s.auth === 'oauth' && error) return { kind: 'signin-failed', error };
    if (!s.signedIn) {
      // account_removed or not_allowed: the page of a failed sign-in, at /signin?error=<reason>.
      if (denied()) return 'redirect';
      if (!s.webSignIn) return { kind: 'open-local' };
      if (!onSignin) return 'redirect';
      const returnTo = params.get('return_to') ?? '/map';
      if (params.get('reason') === 'expired') return { kind: 'expired', returnTo };
      if (params.get('reason') === 'signed_out') return { kind: 'signed-out' };
      return { kind: 'signin', returnTo };
    }
    if (s.user?.deletedAt) return { kind: 'deleting' };
    if (onSignin) return 'redirect';
    return null;
  });

  // Moves the address to the page that applies.
  $effect(() => {
    if (standalone !== 'redirect') return;
    const s = session.value;
    if (!s) return;
    const reason = denied();
    if (!s.signedIn && reason) {
      goto(`/signin?error=${reason}`, { replace: true });
    } else if (s.signedIn) {
      // With sign-in off, or already signed in, /signin goes on to the app.
      goto(params.get('return_to') ?? '/map', { replace: true });
    } else {
      const reason = s.signedOutReason === 'expired' ? '&reason=expired' : '';
      goto(`/signin?return_to=${encodeURIComponent(here)}${reason}`, { replace: true });
    }
  });

  function onkeydown(e: KeyboardEvent) {
    if (standalone) return;
    if (isPaletteShortcut(e)) {
      e.preventDefault();
      palette.show();
    } else if (e.key === 'Escape' && palette.open) {
      palette.hide();
    }
  }
</script>

<svelte:window {onkeydown} />

<QueryClientProvider client={queryClient}>
  {#if standalone === 'redirect'}
    <!-- moving on -->
  {:else if standalone}
    <Standalone page={standalone} />
  {:else}
    <div class="shell">
      <Sidebar />
      {#key store.id}
        {@render children()}
      {/key}
    </div>
    <Palette />
  {/if}
</QueryClientProvider>
