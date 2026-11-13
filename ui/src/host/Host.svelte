<script lang="ts">
  import Setup from "./Setup.svelte";
  import Live from "./Live.svelte";
  import type { HostSession } from "../lib/types";

  let session = $state<HostSession | null>(null);
</script>

{#if session}
  <Live {session} onleave={() => (session = null)} />
{:else}
  <Setup onstart={(s) => (session = s)} />
{/if}
