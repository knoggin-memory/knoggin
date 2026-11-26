<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- The supplied logo files, untouched, as images; app.css crops each to its artwork.
     Light shows the colour file, dark the white one. -->
<script lang="ts">
  let { variant = 'horizontal', alt = '' }: { variant?: 'horizontal' | 'mark' | 'stacked'; alt?: string } = $props();
  const cls = $derived(variant === 'horizontal' ? 'logo' : variant === 'mark' ? 'logo-mark' : 'logo-stacked');
  const file = $derived(variant === 'horizontal' ? 'logo-horizontal' : variant === 'mark' ? 'logo-mark' : 'logo-stacked');
</script>

<span class={cls}><img class="on-light" src="/{file}.svg" {alt} /><img class="on-dark" src="/{file}-white.svg" alt={alt ? alt : ''} /></span>
