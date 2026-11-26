<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The shared dialog: a confirm (title, one paragraph, two buttons) or a form.
     A native modal <dialog>: it traps focus, closes on Esc and sits above everything. -->
<script lang="ts">
  import type { Snippet } from 'svelte';
  let {
    open = $bindable(false),
    title,
    alert = false,
    children,
    actions,
  }: { open?: boolean; title: string; alert?: boolean; children?: Snippet; actions: Snippet } = $props();
  const id = $props.id();
  let el: HTMLDialogElement;
  $effect(() => {
    if (open && !el.open) el.showModal();
    else if (!open && el.open) el.close();
  });
</script>

<dialog bind:this={el} class="dialog" role={alert ? 'alertdialog' : undefined} aria-labelledby="{id}-t" onclose={() => (open = false)}>
  {#if open}
    <h2 class="card-title" id="{id}-t">{title}</h2>
    {@render children?.()}
    <div class="dialog-actions">{@render actions()}</div>
  {/if}
</dialog>
