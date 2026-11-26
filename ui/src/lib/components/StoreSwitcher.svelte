<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The current store: Personal, then the apps, then New app and Manage apps.
     Only active stores are listed. The count is Store.memoryCount. -->
<script lang="ts">
  import { DropdownMenu } from 'bits-ui';
  import { goto } from '$app/navigation';
  import Icon from './Icon.svelte';
  import AppNameDialog from './AppNameDialog.svelte';
  import { store } from '$lib/store.svelte';
  import { storesQuery } from '$lib/queries';
  import { prefs } from '$lib/prefs.svelte';
  import { count } from '$lib/format';

  const stores = storesQuery();
  const active = $derived((stores.data ?? []).filter((s) => s.status === 'active'));
  const current = $derived(active.find((s) => s.id === store.id) ?? (stores.data ?? []).find((s) => s.id === store.id));
  const name = $derived(current?.name ?? (stores.isPending ? '' : 'Personal'));
  const kind = $derived(current?.kind === 'app' ? 'App' : 'Store');
  let open = $state(false);
  let creating = $state(false);

  function onclick(e: MouseEvent) {
    // Closed, the store button opens the sidebar with the menu.
    if (prefs.sidebarCollapsed) {
      e.preventDefault();
      prefs.toggleSidebar();
      setTimeout(() => (open = true), 0);
    }
  }
</script>

<div class="store-switcher">
  <DropdownMenu.Root bind:open>
    <DropdownMenu.Trigger
      class="store-btn"
      aria-label="{kind}: {name}. Switch store"
      title={prefs.sidebarCollapsed ? `${kind}: ${name}` : undefined}
      {onclick}
    >
      <span class="store-name when-open soft-edge"><span class="store-kind">{kind}</span>{name}</span>
      <span class="when-open"><Icon name="up-down" size="var(--icon-sm)" /></span>
      <span class="store-initial when-rail" aria-hidden="true">{name.charAt(0).toUpperCase()}</span>
    </DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content class="menu store-menu" sideOffset={4} align="start" aria-label="Stores">
        <DropdownMenu.RadioGroup value={store.id ?? ''} onValueChange={(v) => v && store.set(v)}>
          {#each active as s, i (s.id)}
            {#if s.kind === 'app' && (i === 0 || active[i - 1].kind !== 'app')}
              <DropdownMenu.Separator class="menu-sep" />
              <span class="micro-label menu-label">Apps</span>
            {/if}
            <DropdownMenu.RadioItem value={s.id} class="menu-item">
              <span class="store-menu-row"><span>{s.name}</span></span><span class="menu-n">{count(s.memoryCount)}</span>
            </DropdownMenu.RadioItem>
          {/each}
        </DropdownMenu.RadioGroup>
        <DropdownMenu.Separator class="menu-sep" />
        <DropdownMenu.Item class="menu-item" onSelect={() => (creating = true)}>New app…</DropdownMenu.Item>
        <DropdownMenu.Item class="menu-item" onSelect={() => goto('/apps')}>Manage apps</DropdownMenu.Item>
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>
</div>

<AppNameDialog bind:open={creating} />
