"use strict";

// La misma interfaz sirve para la app del streamer (Tauri) y para la web de
// los espectadores, que se conecta al repetidor del VPS en /ws.
const native = !!window.__TAURI__;
const invoke = native ? window.__TAURI__.core.invoke : () => Promise.reject(new Error("No disponible en la web"));
const $ = (id) => document.getElementById(id);
const MAX_LINES = 200;
// Segundos por detrás del directo: objetivo, y a partir de cuánto se salta.
const LIVE_TARGET = 0.5;
const LIVE_JUMP = 1.5;

let ws = null;
let mode = null;            // "host" | "viewer"
let statusTimer = null;
let reconnectTimer = null;
let wsUrl = null;

function show(id) {
  for (const s of ["host-setup", "view-setup", "live"]) $(s).classList.toggle("hidden", s !== id);
}

// ---------- Reproductor (MediaSource con MP4 fragmentado) ----------

const player = {
  ms: null, sb: null, queue: [], codec: null, waitKey: false, timer: null,

  reset(codec) {
    this.destroy();
    this.codec = codec;
    const type = `video/mp4; codecs="${codec}"`;
    // En iPhone solo existe ManagedMediaSource (iOS 17.1+).
    const MS = window.ManagedMediaSource || window.MediaSource;
    if (!MS || !MS.isTypeSupported(type)) {
      setPlaceholder(window.MediaSource || window.ManagedMediaSource
        ? `El reproductor no soporta ${codec}. Usa H.264 + AAC en OBS.`
        : "Este navegador no puede reproducir el directo. Actualiza a iOS 17.1 o posterior, o usa otro navegador.");
      return;
    }
    this.ms = new MS();
    const v = $("video");
    // ManagedMediaSource exige desactivar AirPlay si no hay fuente alternativa.
    v.disableRemotePlayback = true;
    this.url = URL.createObjectURL(this.ms);
    v.src = this.url;
    this.ms.addEventListener("sourceopen", () => {
      this.sb = this.ms.addSourceBuffer(type);
      this.sb.mode = "segments";
      this.sb.addEventListener("updateend", () => this.flush());
      this.flush();
    }, { once: true });
    // Revisar también sin datos nuevos: si el vídeo se queda parado en un hueco
    // o se retrasa, flush() lo devuelve al directo.
    this.timer = setInterval(() => this.flush(), 250);
  },

  // kind: 0 = init, 1 = fragmento con keyframe, 2 = fragmento que depende de los anteriores.
  push(kind, buf) {
    if (!this.ms) return;
    if (kind === 0) { this.waitKey = false; this.queue.push(buf); this.flush(); return; }
    // Si se han perdido fragmentos, los siguientes no se pueden decodificar
    // hasta el próximo keyframe.
    if (this.waitKey && kind !== 1) return;
    this.waitKey = false;
    this.queue.push(buf);
    if (this.queue.length > 8) { this.queue = []; this.waitKey = true; }
    this.flush();
  },

  flush() {
    const sb = this.sb, v = $("video");
    if (!sb || sb.updating || this.ms.readyState !== "open") return;
    // Ir pegado al directo: objetivo LIVE_TARGET s por detrás del último dato.
    const b = sb.buffered;
    if (b.length) {
      const start = b.start(0), end = b.end(b.length - 1), behind = end - v.currentTime;
      if (behind > LIVE_JUMP) v.currentTime = end - LIVE_TARGET;
      else {
        // Saltar huecos: si el tiempo actual no cae dentro de ningún rango con
        // datos, el vídeo se quedaría congelado esperando algo que no va a llegar.
        for (let i = 0; i < b.length; i++) {
          if (v.currentTime >= b.start(i) && v.currentTime < b.end(i)) break;
          if (v.currentTime < b.start(i)) { v.currentTime = b.start(i); break; }
        }
      }
      // Pequeños retrasos se recuperan acelerando un poco, sin saltos visibles.
      v.playbackRate = behind > LIVE_TARGET + 0.4 ? 1.08 : 1;
      // Guardar solo unos segundos ya vistos.
      if (v.currentTime - start > 8) { sb.remove(start, v.currentTime - 3); return; }
    }
    const next = this.queue.shift();
    if (!next) return;
    try { sb.appendBuffer(next); }
    catch (e) {
      if (e.name === "QuotaExceededError" && sb.buffered.length) sb.remove(0, v.currentTime - 2);
    }
    if (v.paused && !this.starting) {
      // Sin un toque del usuario, móviles y Safari solo reproducen sin sonido.
      this.starting = true;
      v.play().catch(() => {
        if (v.muted) return;
        v.muted = true;
        sysLine("El navegador empezó el vídeo sin sonido: actívalo con el botón de volumen.");
        return v.play().catch(() => {});
      }).finally(() => { this.starting = false; });
    }
  },

  destroy() {
    clearInterval(this.timer); this.timer = null;
    this.queue = []; this.sb = null; this.codec = null; this.waitKey = false;
    const v = $("video");
    if (this.ms) { v.removeAttribute("src"); v.load(); this.ms = null; }
    if (this.url) { URL.revokeObjectURL(this.url); this.url = null; }
  },
};

function setLive(live, codec) {
  $("badge").textContent = live ? "EN VIVO" : "OFFLINE";
  $("badge").classList.toggle("on", live);
  setPlaceholder(live ? "" : "Esperando señal…");
  if (mode === "host") return;
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
  if (m.sub) {
    const b = document.createElement("span");
    b.className = "sub";
    b.textContent = "SUB";
    b.title = "Suscriptor";
    li.append(b);
  }
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

// wsUrl puede ser una función asíncrona que devuelve la dirección: con cuentas
// de Discord cada conexión necesita un ticket nuevo.
async function connect(url) {
  wsUrl = url;
  clearTimeout(reconnectTimer);
  let target = url;
  if (typeof url === "function") {
    try { target = await url(); }
    catch (e) {
      if (wsUrl !== url) return;
      if (e instanceof AuthError) { authNeeded(e); return; }
      sysLine(String(e.message || e) + " Reintentando…");
      reconnectTimer = setTimeout(() => wsUrl === url && connect(url), 5000);
      return;
    }
    if (wsUrl !== url) return;
  }
  ws = new WebSocket(target);
  ws.binaryType = "arraybuffer";
  ws.onmessage = (e) => {
    if (typeof e.data !== "string") {
      const bytes = new Uint8Array(e.data);
      if (bytes[0] === 0 && !player.ms) return;
      player.push(bytes[0], bytes.subarray(1));
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
      case "auth": onAuthRequired(msg); break;
    }
  };
  ws.onclose = (ev) => {
    if (!wsUrl || ev.target !== ws) return;
    setLive(false);
    sysLine("Conexión perdida, reintentando…");
    const again = wsUrl;
    reconnectTimer = setTimeout(() => wsUrl === again && connect(again), 3000);
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
  clearInterval(subTimer);
  account = null;
  renderAccount();
  $("login-panel").classList.add("hidden");
  player.destroy();
  $("messages").replaceChildren();
  if (mode === "host") await invoke("stop_stream").catch(() => {});
  mode = null;
  $("live").classList.remove("host");
  show(native ? "host-setup" : "view-setup");
}

// ---------- Streamer ----------

function pill(id, cls, text, title) {
  const el = $(id);
  el.className = `pill ${cls}`;
  el.textContent = text;
  el.title = title || "";
}

function fmtTime(ms) {
  const t = Math.floor(ms / 1000), h = Math.floor(t / 3600), m = Math.floor(t / 60) % 60, s = t % 60;
  return (h ? `${h}:` : "") + `${String(m).padStart(h ? 2 : 1, "0")}:${String(s).padStart(2, "0")}`;
}

function renderHealth(st, prev) {
  const stale = st.live && st.now_ms - st.last_data_ms > 3000;
  pill("h-obs", st.live ? (stale ? "bad" : "ok") : "warn",
    st.live ? (stale ? "OBS sin datos" : "OBS conectado") : "Esperando a OBS", st.status);

  let kbps = 0;
  if (prev && st.now_ms > prev.now_ms && st.bytes_total >= prev.bytes_total)
    kbps = ((st.bytes_total - prev.bytes_total) * 8) / (st.now_ms - prev.now_ms);
  pill("h-rate", !st.live ? "" : kbps > 0 ? "ok" : "warn", st.live ? `${Math.round(kbps)} kbps` : "— kbps");

  if (st.tunnel_up === null || st.tunnel_up === undefined) $("h-tunnel").classList.add("hidden");
  else pill("h-tunnel", st.tunnel_up ? "ok" : "bad", st.tunnel_up ? "VPS conectado" : "VPS caído", st.tunnel);

  $("uptime").classList.toggle("hidden", !st.live_since);
  if (st.live_since) $("uptime").textContent = fmtTime(st.now_ms - st.live_since);
  return st;
}

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
    $("live").classList.add("host");
    $("clear").classList.remove("hidden");
    $("health").classList.remove("hidden");
    show("live");
    connect(`ws://127.0.0.1:${s.ws_port}/?k=${s.key}&h=${s.host_token}`);
    let prev = null;
    const tick = async () => {
      const st = await invoke("stream_status").catch(() => null);
      if (st) prev = renderHealth(st, prev);
    };
    tick();
    statusTimer = setInterval(tick, 1000);
  } catch (e) {
    $("host-error").textContent = String(e);
  }
});

$("clear").addEventListener("click", () => invoke("clear_chat"));

// ---------- Espectador ----------

$("join").addEventListener("click", join);

async function join() {
  $("view-error").textContent = "";
  const code = $("code").value.trim();
  localStorage.setItem("name", $("name").value);
  const n = encodeURIComponent($("name").value.trim());
  // Vale la clave sola o el enlace completo.
  const key = code.split("#").pop();
  if (!/^[A-Za-z0-9]{1,64}$/.test(key)) { $("view-error").textContent = "Pega el enlace o la clave del stream."; return; }
  localStorage.setItem("web-key", key);
  viewerBase = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?k=${key}&n=${n}`;
  try {
    mode = "viewer";
    $("clear").classList.add("hidden");
    $("health").classList.add("hidden");
    $("uptime").classList.add("hidden");
    show("live");
    authUrl = null;
    connect(viewerUrl);
  } catch (e) {
    $("view-error").textContent = String(e);
  }
}

// ---------- Cuentas de Discord y suscripción ----------
// Si el repetidor exige cuenta, responde {t:"auth", url} y cierra. La app
// inicia sesión en esa dirección (navegador del sistema), guarda el token de
// sesión y pide un ticket de un solo uso para cada conexión.

class AuthError extends Error {
  constructor(code, invite) { super(code); this.code = code; this.invite = invite; }
}

let viewerBase = null;   // ws://127.0.0.1:PUERTO/?k=…&n=…
let authUrl = null;      // https://… del servicio de cuentas
let account = null;      // { name, sub, payments }
let subTimer = null;

const tokenKey = () => `auth-token:${authUrl}`;
const getToken = () => authUrl && localStorage.getItem(tokenKey());

async function authApi(path, method = "GET") {
  const token = getToken();
  if (!token) throw new AuthError("session");
  let r;
  try { r = await fetch(authUrl + path, { method, headers: { Authorization: `Bearer ${token}` } }); }
  catch { throw new Error("No se pudo contactar con el servicio de cuentas."); }
  const body = await r.json().catch(() => ({}));
  if (r.status === 401) { localStorage.removeItem(tokenKey()); throw new AuthError("session"); }
  if (r.status === 403 && body.error === "not_member") throw new AuthError("not_member", body.invite);
  if (!r.ok) throw new Error(body.error || `Error ${r.status}`);
  return body;
}

async function viewerUrl() {
  if (!authUrl) return viewerBase;
  const [{ ticket }, me] = await Promise.all([authApi("/api/ticket", "POST"), authApi("/api/me")]);
  account = me;
  renderAccount();
  return `${viewerBase}&t=${ticket}`;
}

function onAuthRequired(msg) {
  if (typeof msg.url !== "string" || !msg.url.startsWith("https://")) return;
  if (ws) { ws.onclose = null; ws.close(); }
  if (authUrl !== msg.url) {
    authUrl = msg.url;
    // Primera vez: con la URL del servicio ya se puede pedir un ticket.
    if (getToken() && !msg.retry) { connect(viewerUrl); return; }
  }
  authNeeded(new AuthError("session"));
}

function authNeeded(e) {
  wsUrl = null;
  setLive(false);
  $("login-panel").classList.remove("hidden");
  $("login-msg").textContent = e.code === "not_member"
    ? "Para ver este stream tienes que estar en el servidor de Discord. Únete y vuelve a iniciar sesión."
    : "Este stream es solo para miembros del servidor de Discord.";
  $("login-invite").classList.toggle("hidden", !(e.code === "not_member" && e.invite));
  $("login-invite").dataset.url = e.invite || "";
}


function randomState() {
  const abc = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  return Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => abc[b % abc.length]).join("");
}

async function exchangeCode(code) {
  const r = await fetch(`${authUrl}/api/exchange?code=${encodeURIComponent(code)}`, { method: "POST" });
  const body = await r.json().catch(() => ({}));
  if (!r.ok || !body.token) throw new Error("No se pudo completar el inicio de sesión.");
  localStorage.setItem(tokenKey(), body.token);
}

// En la web, Discord vuelve a esta misma página con ?code=…&state=….
async function finishWebLogin() {
  const q = new URLSearchParams(location.search);
  if (!q.has("code")) return;
  const expected = localStorage.getItem("login-state");
  localStorage.removeItem("login-state");
  history.replaceState(null, "", location.pathname + location.hash);
  if (!expected || q.get("state") !== expected) return;
  authUrl = location.origin;
  try { await exchangeCode(q.get("code")); }
  catch (e) { $("view-error").textContent = String(e.message || e); }
}

// Discord vuelve a esta misma página con ?code=…&state=….
$("login-discord").addEventListener("click", () => {
  const state = randomState();
  localStorage.setItem("login-state", state);
  location.href = `${authUrl}/login?state=${state}`;
});

$("login-invite").addEventListener("click", () => {
  const url = $("login-invite").dataset.url;
  if (url) openUrl(url).catch(() => {});
});

// En otra pestaña; si el navegador no deja, en esta misma.
async function openUrl(url) {
  const w = window.open(url, "_blank");
  if (w) w.opener = null;
  else location.href = url;
}

function renderAccount() {
  const show = !!account;
  $("account").classList.toggle("hidden", !show);
  if (!show) return;
  $("account-name").textContent = account.name;
  $("account-sub").classList.toggle("hidden", !account.sub);
  $("subscribe").classList.toggle("hidden", account.sub || !account.payments);
}

$("subscribe").addEventListener("click", async () => {
  try {
    const { url } = await authApi("/api/checkout", "POST");
    await openUrl(url);
    sysLine("Completa el pago en el navegador. La insignia aparecerá al confirmarse.");
    // Al confirmarse el pago, reconectar para entrar con la insignia.
    clearInterval(subTimer);
    let tries = 0;
    subTimer = setInterval(async () => {
      if (++tries > 120 || !authUrl) { clearInterval(subTimer); return; }
      const me = await authApi("/api/me").catch(() => null);
      if (me && me.sub) {
        clearInterval(subTimer);
        account = me;
        renderAccount();
        sysLine("¡Gracias por suscribirte!");
        if (ws) ws.close();
      }
    }, 5000);
  } catch (e) {
    if (e instanceof AuthError) authNeeded(e);
    else sysLine(String(e.message || e));
  }
});

$("logout").addEventListener("click", async () => {
  await authApi("/api/logout", "POST").catch(() => {});
  localStorage.removeItem(tokenKey());
  account = null;
  renderAccount();
  if (ws) { ws.onclose = null; ws.close(); }
  authNeeded(new AuthError("session"));
});

// ---------- Navegación ----------

let keys = null;

function showKeys(k) {
  keys = k;
  $("k-viewer").textContent = k.viewer_key;
  $("k-stream").textContent = k.stream_key;
  $("k-rtmp").textContent = `rtmp://127.0.0.1:${Number($("rtmp-port").value)}/live`;
  // La web vive en el VPS, con el certificado de <ip-con-guiones>.sslip.io.
  const host = $("vps").value.trim().split("@").pop();
  const web = /^\d+\.\d+\.\d+\.\d+$/.test(host) ? `${host.replace(/\./g, "-")}.sslip.io` : host;
  $("k-web").textContent = web ? `https://${web}/#${k.viewer_key}` : "Indica el VPS";
}

for (const id of ["vps", "ws-port", "rtmp-port"])
  $(id).addEventListener("input", () => keys && showKeys(keys));

async function regen(which, msg) {
  if (!confirm(msg)) return;
  try { showKeys(await invoke("regenerate_keys", { which })); }
  catch (e) { $("host-error").textContent = String(e); }
}

$("regen-viewer").addEventListener("click", () =>
  regen("viewer", "¿Generar una clave de espectadores nueva? El código anterior dejará de funcionar."));
$("regen-stream").addEventListener("click", () =>
  regen("stream", "¿Generar una clave de stream nueva? Tendrás que pegarla otra vez en OBS."));

$("leave").addEventListener("click", leave);

function applyLayout(stacked) {
  $("live").classList.toggle("stacked", stacked);
  $("layout").textContent = stacked ? "Chat al lado" : "Chat abajo";
  localStorage.setItem("stacked", stacked ? "1" : "0");
}
$("layout").addEventListener("click", () => applyLayout(!$("live").classList.contains("stacked")));
applyLayout(localStorage.getItem("stacked") === "1");
document.querySelectorAll("[data-copy]").forEach((b) =>
  b.addEventListener("click", () => navigator.clipboard.writeText($(b.dataset.copy).textContent)));
$("name").value = localStorage.getItem("name") || "";
$("vps").value = localStorage.getItem("vps") ?? "stream@109.123.252.126";
$("vps-key").value = localStorage.getItem("vps-key") ?? "~/.ssh/mestisexo_tunnel";

if (native) {
  show("host-setup");
  invoke("get_keys").then(showKeys, (e) => { $("host-error").textContent = String(e); });
} else {
  document.body.classList.add("web");
  $("code").value = location.hash.slice(1) || localStorage.getItem("web-key") || "";
  show("view-setup");
  finishWebLogin().then(() => { if ($("code").value) join(); });
}
