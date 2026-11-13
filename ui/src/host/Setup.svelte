<script lang="ts">
  import { onMount } from "svelte";
  import { invoke } from "../lib/tauri";
  import { webLink } from "../lib/health";
  import type { HostSession, Keys } from "../lib/types";

  let { onstart }: { onstart: (s: HostSession) => void } = $props();

  let wsPort = $state(8090);
  let rtmpPort = $state(1935);
  let vps = $state(localStorage.getItem("vps") ?? "stream@109.123.252.126");
  let vpsKey = $state(localStorage.getItem("vps-key") ?? "~/.ssh/mestisexo_tunnel");
  let keys = $state<Keys | null>(null);
  let error = $state("");

  const rtmp = $derived(`rtmp://127.0.0.1:${Number(rtmpPort)}/live`);
  const web = $derived(webLink(vps) ?? "Indica el VPS");

  onMount(() => {
    invoke<Keys>("get_keys").then((k) => (keys = k), (e) => (error = String(e)));
  });

  async function regen() {
    if (!confirm("¿Generar una clave de stream nueva? Tendrás que pegarla otra vez en OBS.")) return;
    try {
      keys = await invoke<Keys>("regenerate_keys", { which: "stream" });
    } catch (e) {
      error = String(e);
    }
  }

  async function start() {
    error = "";
    localStorage.setItem("vps", vps);
    localStorage.setItem("vps-key", vpsKey);
    try {
      onstart(await invoke<HostSession>("start_stream", {
        wsPort: Number(wsPort),
        rtmpPort: Number(rtmpPort),
        vps: vps.trim() || null,
        vpsKey: vpsKey.trim() || null,
      }));
    } catch (e) {
      error = String(e);
    }
  }

  const copy = (text: string) => navigator.clipboard.writeText(text);
</script>

<section class="screen">
  <h2>Transmitir</h2>
  <label>Puerto local <input bind:value={wsPort} type="number" min="1" max="65535"></label>
  <label>Puerto RTMP (OBS) <input bind:value={rtmpPort} type="number" min="1" max="65535"></label>
  <label>VPS <input bind:value={vps} placeholder="stream@109.123.252.126" autocomplete="off"></label>
  <label>Clave SSH del VPS <input bind:value={vpsKey} placeholder="~/.ssh/mestisexo_tunnel" autocomplete="off"></label>
  <div class="keys">
    <div><span>Clave de stream (OBS)</span><code class="secret">{keys?.stream_key ?? ""}</code><button onclick={regen}>Nueva</button></div>
    <p class="muted">Es fija entre sesiones. Si generas una nueva, tendrás que pegarla otra vez en OBS.</p>
    <div><span>Servidor (OBS)</span><code>{keys ? rtmp : ""}</code><button onclick={() => copy(rtmp)}>Copiar</button></div>
    <div><span>Enlace para espectadores</span><code>{keys ? web : ""}</code><button onclick={() => copy(web)}>Copiar</button></div>
  </div>
  <div class="row">
    <button class="primary" onclick={start}>Iniciar sesión</button>
  </div>
  <p class="error">{error}</p>
</section>
