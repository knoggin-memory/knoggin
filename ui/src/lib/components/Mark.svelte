<!-- The bulb mark (DESIGN §9a). Drawn as in ui/static/logo.svg. -->
<script lang="ts">
  let { size = 'var(--mark)' }: { size?: string } = $props();
</script>

<svg width={size} height={size} viewBox="0 0 24 24" fill="none" aria-hidden="true">
  <path
    class="glass"
    d="M12 2.7C8.1 2.6 5.1 5.6 5.3 9.4c0.1 2.5 1.3 4.1 2.6 5.4c0.8 0.8 1.2 1.6 1.3 2.7h5.6c0.1-1.1 0.6-1.9 1.4-2.7c1.4-1.4 2.6-3 2.5-5.6C18.6 5.5 15.8 2.8 12 2.7z"
    stroke-width="1.8"
    stroke-linejoin="round"
  />
  <circle class="dot" cx="12" cy="10" r="2.2" />
  <path class="ink" d="M9.6 20.4h4.8" stroke-width="1.8" stroke-linecap="round" />
</svg>

<style>
  .glass {
    fill: var(--color-mark-glass);
    stroke: var(--color-accent-strong);
  }
  .dot {
    fill: var(--color-accent);
  }
  .ink {
    stroke: var(--color-accent-strong);
  }
</style>
