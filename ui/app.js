"use strict";

const invoke = window.__TAURI__.core.invoke;
const $ = (id) => document.getElementById(id);
const MAX_LINES = 200;

let ws = null;
let mode = null;            // "host" | "viewer"
let statusTimer = null;
let reconnectTimer = null;
let wsUrl = null;

function show(id) {
  for (const s of ["home", "host-setup", "view-setup", "live"]) $(s).classList.toggle("hidden", s !== id);
}

// ---------- Reproductor (MediaSource con MP4 fragmentado) ----------

const player = {
  ms: null, sb: null, queue: [], codec: null,

  reset(codec) {
    this.destroy();
    this.codec = codec;
    const type = `video/mp4; codecs="${codec}"`;
    if (!window.MediaSource || !MediaSource.isTypeSupported(type)) {
      setPlaceholder(`El reproductor no soporta ${codec}. Usa H.264 + AAC en OBS.`);
      return;
    }
    this.ms = new MediaSource();
    $("video").src = URL.createObjectURL(this.ms);
    this.ms.addEventListener("sourceopen", () => {
      this.sb = this.ms.addSourceBuffer(type);
      this.sb.mode = "segments";
      this.sb.addEventListener("updateend", () => this.flush());
      this.flush();
    }, { once: true });
  },

  push(buf) {
    if (!this.ms) return;
    this.queue.push(buf);
    if (this.queue.length > 60) this.queue.splice(0, this.queue.length - 30);
    this.flush();
  },

  flush() {
    const sb = this.sb, v = $("video");
    if (!sb || sb.updating || this.ms.readyState !== "open") return;
    // Mantener el búfer corto y seguir el directo.
    if (sb.buffered.length) {
      const start = sb.buffered.start(0), end = sb.buffered.end(sb.buffered.length - 1);
      if (v.currentTime < start || end - v.currentTime > 4) v.currentTime = Math.max(start, end - 1);
      if (v.currentTime - start > 30) { sb.remove(start, v.currentTime - 10); return; }
    }
    const next = this.queue.shift();
    if (!next) return;
    try { sb.appendBuffer(next); }
    catch (e) {
      if (e.name === "QuotaExceededError" && sb.buffered.length) sb.remove(0, v.currentTime - 2);
    }
    if (v.paused) v.play().catch(() => {});
  },

  destroy() {
    this.queue = []; this.sb = null; this.codec = null;
    const v = $("video");
    if (this.ms) { v.removeAttribute("src"); v.load(); this.ms = null; }
  },
};

function setLive(live, codec) {
  $("badge").textContent = live ? "EN VIVO" : "OFFLINE";
  $("badge").classList.toggle("on", live);
  setPlaceholder(live ? "" : "Esperando señal…");
  if (live && codec) player.reset(codec);
  if (!live) player.destroy();
}

function setPlaceholder(text) {
  $("placeholder").textContent = text;
  $("placeholder").classList.toggle("hidden", !text);
}

// ---------- Chat ----------

function addLine(m) {
  const li = document.createElement("li");
  if (m.host) li.className = "host";
  const n = document.createElement("span");
  n.className = "n";
  n.textContent = m.name + ":";
  n.style.color = colorFor(m.name);
  li.append(n, document.createTextNode(m.text));
  li.title = new Date(m.ts).toLocaleTimeString();
  appendLi(li);
}

function sysLine(text) {
  const li = document.createElement("li");
  li.className = "sys";
  li.textContent = text;
  appendLi(li);
}

function appendLi(li) {
  const list = $("messages");
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  list.append(li);
  while (list.children.length > MAX_LINES) list.firstChild.remove();
  if (atBottom) list.scrollTop = list.scrollHeight;
}

function colorFor(name) {
  let h = 0;
  for (const c of name) h = (h * 31 + c.codePointAt(0)) >>> 0;
  return `hsl(${h % 360} 70% 65%)`;
}

$("chat-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const text = $("chat-input").value.trim();
  if (!text || !ws || ws.readyState !== WebSocket.OPEN) return;
  ws.send(JSON.stringify({ t: "chat", text }));
  $("chat-input").value = "";
});

// ---------- Conexión WebSocket ----------

function connect(url) {
  wsUrl = url;
  clearTimeout(reconnectTimer);
  ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";
  ws.onmessage = (e) => {
    if (typeof e.data !== "string") {
      const bytes = new Uint8Array(e.data);
      if (bytes[0] === 0 && !player.ms) return;
      player.push(bytes.subarray(1));
      return;
    }
    const msg = JSON.parse(e.data);
    switch (msg.t) {
      case "welcome":
        $("messages").replaceChildren();
        msg.history.forEach(addLine);
        sysLine("Conectado al chat. Los mensajes se borran al terminar la sesión.");
        setViewers(msg.viewers);
        setLive(msg.live, msg.codec);
        break;
      case "chat": addLine(msg.m); break;
      case "viewers": setViewers(msg.n); break;
      case "live": setLive(true, msg.codec); break;
      case "offline": setLive(false); break;
      case "clear": $("messages").replaceChildren(); sysLine("El streamer vació el chat."); break;
      case "end": sysLine("La sesión ha terminado."); setLive(false); wsUrl = null; break;
    }
  };
  ws.onclose = (ev) => {
    if (!wsUrl || ev.target !== ws) return;
    setLive(false);
    sysLine("Conexión perdida, reintentando…");
    reconnectTimer = setTimeout(() => wsUrl && connect(wsUrl), 3000);
  };
}

function setViewers(n) {
  $("viewers").textContent = `${n} espectador${n === 1 ? "" : "es"}`;
}

async function leave() {
  wsUrl = null;
  clearTimeout(reconnectTimer);
  clearInterval(statusTimer);
  if (ws) { ws.onclose = null; ws.close(); ws = null; }
  player.destroy();
  $("messages").replaceChildren();
  if (mode === "host") await invoke("stop_stream").catch(() => {});
  if (mode === "viewer") await invoke("disconnect_viewer").catch(() => {});
  mode = null;
  show("home");
}

// ---------- Streamer ----------

$("start").addEventListener("click", async () => {
  $("host-error").textContent = "";
  localStorage.setItem("vps", $("vps").value);
  localStorage.setItem("vps-key", $("vps-key").value);
  try {
    const s = await invoke("start_stream", {
      wsPort: Number($("ws-port").value),
      rtmpPort: Number($("rtmp-port").value),
      vps: $("vps").value.trim() || null,
      vpsKey: $("vps-key").value.trim() || null,
    });
    mode = "host";
    const ip = s.vps_host || s.tailscale_ip || s.lan_ip || "TU_IP";
    $("i-rtmp").textContent = s.rtmp_server;
    $("i-skey").textContent = s.stream_key;
    $("i-code").textContent = `${ip}:${s.ws_port}#${s.key}`;
    $("i-net").textContent = s.vps_host
      ? "Los espectadores se conectan a tu VPS; tu IP no se muestra."
      : s.tailscale_ip
      ? "Código con tu IP de Tailscale: los espectadores necesitan Tailscale y acceso a tu equipo (Share)."
      : `Sin VPS ni Tailscale: el código solo sirve en tu red local.`;
    $("host-info").classList.remove("hidden");
    $("clear").classList.remove("hidden");
    show("live");
    connect(`ws://127.0.0.1:${s.ws_port}/?k=${s.key}&h=${s.host_token}`);
    statusTimer = setInterval(async () => {
      const st = await invoke("stream_status").catch(() => null);
      if (st) $("status").textContent = st.tunnel ? `${st.status} · ${st.tunnel}` : st.status;
    }, 1000);
  } catch (e) {
    $("host-error").textContent = String(e);
  }
});

$("clear").addEventListener("click", () => invoke("clear_chat"));

// ---------- Espectador ----------

$("join").addEventListener("click", async () => {
  $("view-error").textContent = "";
  const [addr, key] = $("code").value.trim().split("#");
  if (!addr || !key) { $("view-error").textContent = "El código debe tener el formato ip:puerto#clave"; return; }
  localStorage.setItem("name", $("name").value);
  try {
    const port = await invoke("connect_viewer", { addr });
    mode = "viewer";
    $("host-info").classList.add("hidden");
    $("clear").classList.add("hidden");
    $("status").textContent = `Conectado a ${addr}`;
    show("live");
    const n = encodeURIComponent($("name").value.trim());
    connect(`ws://127.0.0.1:${port}/?k=${encodeURIComponent(key)}&n=${n}`);
  } catch (e) {
    $("view-error").textContent = String(e);
  }
});

// ---------- Navegación ----------

$("go-host").addEventListener("click", () => show("host-setup"));
$("go-view").addEventListener("click", () => show("view-setup"));
document.querySelectorAll(".back").forEach((b) => b.addEventListener("click", () => show("home")));
$("leave").addEventListener("click", leave);
document.querySelectorAll("[data-copy]").forEach((b) =>
  b.addEventListener("click", () => navigator.clipboard.writeText($(b.dataset.copy).textContent)));
$("name").value = localStorage.getItem("name") || "";
$("vps").value = localStorage.getItem("vps") ?? "stream@109.123.252.126";
$("vps-key").value = localStorage.getItem("vps-key") ?? "~/.ssh/mestisexo_tunnel";
