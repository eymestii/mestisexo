<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import Chat from "../components/Chat.svelte";
  import { ChatConnection } from "../lib/chat.svelte";
  import { invoke } from "../lib/tauri";
  import type { HostSession } from "../lib/types";

  // Streamer: solo el chat. El vídeo no se le envía, así que su PC no gasta en
  // decodificarlo; para ver cómo queda, abre la web.

  let { session, onleave }: { session: HostSession; onleave: () => void } = $props();

  let live = $state(false);

  const chat = new ChatConnection({ onLive: (on) => (live = on) });

  onMount(() => {
    chat.connect(`ws://127.0.0.1:${session.ws_port}/?k=${session.key}&h=${session.host_token}`);
  });

  onDestroy(() => chat.stop());

  async function leave() {
    chat.stop();
    await invoke("stop_stream").catch(() => {});
    onleave();
  }
</script>

<section id="live" class="host">
  <header>
    <button onclick={leave}>Salir</button>
    <span class="badge" class:on={live}>{live ? "EN VIVO" : "OFFLINE"}</span>
    <span id="viewers" title="Espectadores">{chat.viewers}</span>
  </header>
  <div class="main">
    <Chat {chat} />
  </div>
</section>
