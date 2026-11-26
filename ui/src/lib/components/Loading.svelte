<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- A screen's first fetch: the quiet loading line in the screen's own area. -->
<script lang="ts">
  import Logo from './Logo.svelte';
</script>

<div class="wait breathing" role="status">
  <Logo variant="mark" />
  <h2>Loading.</h2>
  <div class="progress indeterminate"><span></span></div>
</div>
