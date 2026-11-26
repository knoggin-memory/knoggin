<!-- First chip of every toolbar: the global namespace scope (DESIGN §2, §5). -->
<script lang="ts">
  import { DropdownMenu } from 'bits-ui';
  import Icon from './Icon.svelte';
  import { scope } from '$lib/scope.svelte';
  import { namespacesQuery } from '$lib/queries';

  // By default the chip is the global scope. Search passes its own `value` and
  // `onchange` so its filter starts from the page's scope without changing it.
  let { value = undefined, onchange = undefined }: { value?: string | null; onchange?: (namespace: string | null) => void } = $props();
  const current = $derived(onchange ? (value ?? null) : scope.current);
  const choose = (namespace: string | null) => (onchange ? onchange(namespace) : scope.set(namespace));
  const namespaces = namespacesQuery();
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger class="chip scope-chip" title="Namespace scope: also set from the sidebar">
    <Icon name="layers" stroke={1.7} size="var(--icon-sm)" />{current ?? 'All namespaces'}<Icon name="chevron" stroke={1.8} size="var(--icon-sm)" />
  </DropdownMenu.Trigger>
  <DropdownMenu.Portal>
    <DropdownMenu.Content class="menu" sideOffset={6} align="start">
      <DropdownMenu.RadioGroup value={current ?? ''} onValueChange={(v) => choose(v || null)}>
        <DropdownMenu.RadioItem value="" class="menu-item">All namespaces</DropdownMenu.RadioItem>
        {#each namespaces.data ?? [] as ns (ns.id)}
          <DropdownMenu.RadioItem value={ns.name} class="menu-item"><span>{ns.name}</span><span class="mono menu-n">{ns.memoryCount}</span></DropdownMenu.RadioItem>
        {/each}
      </DropdownMenu.RadioGroup>
    </DropdownMenu.Content>
  </DropdownMenu.Portal>
</DropdownMenu.Root>

<style>
  :global(.scope-chip) {
    background: var(--color-surface) !important;
    border-color: var(--color-accent-soft-border) !important;
    color: var(--color-text) !important;
    gap: calc(var(--space-1) * 1.5) !important;
  }
  :global(.menu) {
    min-width: calc(var(--sidebar-width) + var(--space-8));
    padding: var(--space-1);
    background: var(--color-surface);
    border: var(--hairline) solid var(--color-border);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-popover);
    /* Above the ⌘K palette (40/41), which opens this menu for its own filter. */
    z-index: 50;
  }
  :global(.menu-item) {
    display: flex;
    justify-content: space-between;
    gap: var(--space-4);
    padding: var(--space-2) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-control);
    color: var(--color-text-2);
    cursor: pointer;
    outline: none;
  }
  :global(.menu-item[data-highlighted]) {
    background: var(--color-accent-soft);
  }
  :global(.menu-item[data-state='checked']) {
    color: var(--color-accent-strong);
    font-weight: var(--weight-semibold);
  }
  :global(.menu-n) {
    color: var(--color-text-muted);
    font-size: var(--text-value);
  }
</style>
