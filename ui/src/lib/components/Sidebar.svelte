<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- One <aside> in both states: closing it is a drawer, not a swap (handoff,
     "Sidebar motion"). Labels and icons dissolve into each other in place. -->
<script lang="ts">
  import { page } from '$app/state';
  import { goto } from '$app/navigation';
  import Icon, { type IconName } from './Icon.svelte';
  import Logo from './Logo.svelte';
  import StoreSwitcher from './StoreSwitcher.svelte';
  import { scope } from '$lib/scope.svelte';
  import { prefs } from '$lib/prefs.svelte';
  import { session, who } from '$lib/session.svelte';
  import { status } from '$lib/status.svelte';
  import { memoryTotal, namespacesQuery } from '$lib/queries';
  import { count } from '$lib/format';

  const views: { href: string; label: string; icon: IconName }[] = [
    { href: '/map', label: 'Memory map', icon: 'map' },
    { href: '/explore', label: 'Explore', icon: 'explore' },
    { href: '/timeline', label: 'Timeline', icon: 'timeline' },
    { href: '/health', label: 'Health', icon: 'health' },
    { href: '/keys', label: 'API keys', icon: 'key' },
  ];
  const manage = $derived(
    [
      { href: '/apps', label: 'Apps', icon: 'apps' as IconName, show: true },
      { href: '/setup', label: 'Set up an agent', icon: 'plug' as IconName, show: true },
      { href: '/users', label: 'Users', icon: 'users' as IconName, show: session.server && session.signInOn && session.isAdmin },
      { href: '/status', label: 'Server status', icon: 'server' as IconName, show: session.server && session.isAdmin },
    ].filter((v) => v.show),
  );

  const namespaces = namespacesQuery();
  const total = $derived(memoryTotal(namespaces.data ?? [], null));
  const current = (href: string) => page.url.pathname === href || page.url.pathname.startsWith(href + '/');
  const collapsed = $derived(prefs.sidebarCollapsed);
  // The profile row and Sign out exist only for a cookie session.
  const profile = $derived(session.cookieSession && session.value?.user ? who(session.value.user) : null);
  const statusLine = $derived(session.server ? 'connected' : status.notRunning ? 'daemon unavailable' : 'daemon connected');
</script>

<aside class="sidebar" class:is-collapsed={collapsed} aria-label="Sidebar">
  <div class="brand">
    <!-- One element in both states, so the lockup can close down to its mark. Closed, it opens the sidebar. -->
    <button
      type="button"
      class="logo-btn"
      aria-label={collapsed ? 'Expand sidebar' : 'Knoggin: Memory map'}
      title={collapsed ? 'Expand sidebar' : undefined}
      onclick={() => (collapsed ? prefs.toggleSidebar() : goto('/map'))}
    >
      <Logo />
    </button>
    <button type="button" class="icon-btn collapse when-open" aria-label="Collapse sidebar" aria-expanded="true" onclick={() => prefs.toggleSidebar()}>
      <Icon name="collapse" stroke={1.8} />
    </button>
  </div>

  <StoreSwitcher />

  <nav aria-label="Views" class="views">
    {#each views as v (v.href)}
      <a href={v.href} class="nav-link" aria-current={current(v.href) ? 'page' : undefined} title={collapsed ? v.label : undefined}>
        <span class="nav-ico when-rail" aria-hidden="true"><Icon name={v.icon} /></span><span class="nav-label when-open soft-edge">{v.label}</span>
      </a>
    {/each}
  </nav>

  <div role="group" aria-label="Namespace scope" class="namespaces when-open keep-width">
    <span class="micro-label heading">Namespaces</span>
    <button type="button" class="ns-btn" aria-pressed={scope.current === null} onclick={() => scope.set(null)}>
      <span class="ns-name">All namespaces</span><span class="n">{namespaces.data ? count(total) : ''}</span>
    </button>
    {#if namespaces.isPending}
      {#each [0, 1, 2] as i (i)}<span class="skeleton ns-skel"></span>{/each}
    {/if}
    {#each namespaces.data ?? [] as ns (ns.name)}
      <button type="button" class="ns-btn" aria-pressed={scope.current === ns.name} onclick={() => scope.set(ns.name)}>
        <span class="ns-name">{ns.name}</span><span class="n">{count(ns.memoryCount)}</span>
      </button>
    {/each}
  </div>

  <div class="sidebar-foot">
    <nav aria-label="Manage" class="views">
      {#each manage as v (v.href)}
        <a href={v.href} class="nav-link" aria-current={current(v.href) ? 'page' : undefined} title={collapsed ? v.label : undefined}>
          <span class="nav-ico when-rail" aria-hidden="true"><Icon name={v.icon} /></span><span class="nav-label when-open soft-edge">{v.label}</span>
        </a>
      {/each}
      {#if profile}
        <a href="/account" class="nav-link account-link" aria-current={current('/account') ? 'page' : undefined} title={collapsed ? 'Account' : undefined}>
          <span class="avatar" aria-hidden="true">{profile.initials}</span><span class="who when-open soft-edge">{profile.text}</span>
        </a>
      {:else}
        <a href="/account" class="nav-link" aria-current={current('/account') ? 'page' : undefined} title={collapsed ? 'Settings' : undefined}>
          <span class="nav-ico when-rail" aria-hidden="true"><Icon name="user" /></span><span class="nav-label when-open soft-edge">Settings</span>
        </a>
      {/if}
    </nav>
    {#if session.server && !session.signInOn}
      <span class="caption when-open keep-width">Sign-in is off. Anyone who can reach this address has access.</span>
    {/if}
    <span class="status when-open keep-width" title="Only reinforcing a memory slows its decay. Browsing here changes nothing.">{statusLine}</span>
  </div>
</aside>

<style>
  .logo-btn {
    display: block;
    padding: 0;
    border: 0;
    background: none;
    cursor: pointer;
    border-radius: var(--radius-sm);
  }
  .ns-skel {
    height: var(--space-4);
    margin: var(--space-2) 0;
  }
</style>
