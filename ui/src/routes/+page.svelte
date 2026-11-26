<!-- SPDX-License-Identifier: AGPL-3.0-only -->
<!-- `/` goes to the map when there is a session (the layout handles every other case). -->
<script lang="ts">
  import { goto } from '$app/navigation';
  $effect(() => {
    goto('/map', { replace: true });
  });
</script>
