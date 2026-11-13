<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import Chat from "../components/Chat.svelte";
  import { ChatConnection, viewersLabel } from "../lib/chat.svelte";
  import { health, type Health } from "../lib/health";
  import { invoke } from "../lib/tauri";
  import type { HostSession, Status } from "../lib/types";

  // Streamer: solo el chat y el estado. El vídeo no se le envía, así que su PC
  // no gasta en decodificarlo; para ver cómo queda, abre la web.

  let { session, onleave }: { session: HostSession; onleave: () => void } = $props();

  let live = $state(false);
  let h = $state<Health | null>(null);
  let timer: ReturnType<typeof setInterval> | undefined;

  const chat = new ChatConnection({ onLive: (on) => (live = on) });

  onMount(() => {
    chat.connect(`ws://127.0.0.1:${session.ws_port}/?k=${session.key}&h=${session.host_token}`);
    const tick = async () => {
      const st = await invoke<Status | null>("stream_status").catch(() => null);
      if (st) h = health(st);
    };
    tick();
    timer = setInterval(tick, 1000);
  });

  onDestroy(() => {
    clearInterval(timer);
    chat.stop();
  });

  async function leave() {
    clearInterval(timer);
    chat.stop();
    await invoke("stop_stream").catch(() => {});
    onleave();
  }
</script>

<section id="live" class="host">
  <header>
    <button onclick={leave}>Salir</button>
    <span class="badge" class:on={live}>{live ? "EN VIVO" : "OFFLINE"}</span>
    {#if h?.uptime}<span id="uptime">{h.uptime}</span>{/if}
    <span id="viewers">{viewersLabel(chat.viewers)}</span>
    {#if h?.quality}
      <span class="pill {h.quality.cls}" title={h.quality.title}>{h.quality.text}</span>
    {/if}
  </header>
  <div class="main">
    <Chat {chat} />
  </div>
</section>
