<!-- Menu chip with a multi-select listbox (Tags, Entities, Agent, Links). -->
<script lang="ts">
  import { DropdownMenu } from 'bits-ui';
  import Icon from './Icon.svelte';

  let {
    label,
    options,
    selected = $bindable([]),
    empty = 'Nothing to filter by yet',
    showCount = true,
  }: {
    label: string;
    options: { value: string; label?: string; count?: number }[];
    selected?: string[];
    empty?: string;
    /** Append " · n selected" to the label. */
    showCount?: boolean;
  } = $props();

  function toggle(v: string) {
    selected = selected.includes(v) ? selected.filter((x) => x !== v) : [...selected, v];
  }
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger class="chip menu-chip" aria-pressed={showCount && selected.length > 0}>
    {label}{showCount && selected.length ? ` · ${selected.length}` : ''}<Icon name="chevron" stroke={1.8} size="var(--icon-sm)" />
  </DropdownMenu.Trigger>
  <DropdownMenu.Portal>
    <DropdownMenu.Content class="menu scroll" sideOffset={6} align="start">
      {#each options as o (o.value)}
        <DropdownMenu.CheckboxItem class="menu-item" checked={selected.includes(o.value)} onCheckedChange={() => toggle(o.value)} closeOnSelect={false}>
          {#snippet children({ checked })}
            <span class:on={checked}>{o.label ?? o.value}</span>{#if o.count !== undefined}<span class="mono menu-n">{o.count}</span>{/if}
          {/snippet}
        </DropdownMenu.CheckboxItem>
      {:else}
        <div class="menu-item muted">{empty}</div>
      {/each}
      {#if selected.length}
        <DropdownMenu.Separator class="menu-sep" />
        <DropdownMenu.Item class="menu-item" onSelect={() => (selected = [])}>Clear</DropdownMenu.Item>
      {/if}
    </DropdownMenu.Content>
  </DropdownMenu.Portal>
</DropdownMenu.Root>

<style>
  .on {
    color: var(--color-accent-strong);
    font-weight: var(--weight-semibold);
  }
  .muted {
    color: var(--color-text-muted);
  }
  :global(.menu.scroll) {
    max-height: calc(var(--palette-top) * 4);
    overflow-y: auto;
  }
  :global(.menu-sep) {
    height: var(--hairline);
    background: var(--color-divider);
    margin: var(--space-1) 0;
  }
  :global(.menu-chip) {
    padding-right: calc(var(--space-2) + var(--space-1) / 2);
  }
</style>
