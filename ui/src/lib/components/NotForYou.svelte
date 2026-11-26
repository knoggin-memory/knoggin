<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A page that is not for this person: its sentence and a way back to the map. -->
<script lang="ts">
  let { title, sentence }: { title: string; sentence: string } = $props();
</script>

<svelte:head><title>Knoggin · {title}</title></svelte:head>
<header class="header"><div class="title"><h1>{title}</h1></div></header>
<div class="empty"><p>{sentence}</p><a class="btn-secondary" href="/map">Back to the Memory map</a></div>
