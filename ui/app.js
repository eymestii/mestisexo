"use strict";

// La misma interfaz sirve para la app del streamer (Tauri) y para la web de
// los espectadores, que reproduce el directo por LL-HLS y usa /ws solo para
// el chat.
const native = !!window.__TAURI__;
const invoke = native ? window.__TAURI__.core.invoke : () => Promise.reject(new Error("No disponible en la web"));
const $ = (id) => document.getElementById(id);
const MAX_LINES = 200;

let ws = null;
let mode = null;            // "host" | "viewer"
let statusTimer = null;
let reconnectTimer = null;
let wsUrl = null;

function show(id) {
  for (const s of ["host-setup", "live"]) $(s).classList.toggle("hidden", s !== id);
}

// ---------- Reproductor (LL-HLS) ----------
// El vídeo llega por HTTP en trozos que cualquier CDN puede cachear, así que
// aquí no hay control de búfer propio: la latencia la gobierna la playlist
// (PART-HOLD-BACK) y la gestiona hls.js, o Safari por su cuenta.

const player = {
  hls: null, url: null,

  // `url` es la playlist, ya con el token de acceso.
  async start(url) {
    if (this.url === url) return;
    this.destroy();
    this.url = url;
    const v = $("video");
    // Safari reproduce HLS por sí mismo y lo hace mejor que cualquier
    // biblioteca: menos batería en iPhone y pantalla completa nativa.
    if (v.canPlayType("application/vnd.apple.mpegurl")) {
      v.src = url;
      // Safari no avisa de por qué falla: si el token caducó, lo único visible
      // es un error del elemento de vídeo, así que se renueva y se reintenta.
      v.onerror = () => refreshVideo();
      this.play();
      return;
    }
    const Hls = await loadHls();
    if (!Hls || !Hls.isSupported()) {
      setPlaceholder("Este navegador no puede reproducir el directo. Actualiza a iOS 17.1 o posterior, o usa otro navegador.");
      return;
    }
    this.hls = new Hls({
      lowLatencyMode: true,
      backBufferLength: 10,
      // Con las partes de 0,25 s el reproductor se pega al directo; si la red
      // de este espectador no da, hls.js se retrasa solo y vuelve luego.
      liveSyncDurationCount: 1,
      maxLiveSyncPlaybackRate: 1.05,
      manifestLoadingMaxRetry: 6,
      levelLoadingMaxRetry: 6,
      fragLoadingMaxRetry: 6,
    });
    this.hls.on(Hls.Events.ERROR, (_, data) => {
      if (!data.fatal) return;
      // Un error de red con el directo en marcha casi siempre es el token
      // caducado o un corte: se pide la playlist otra vez.
      if (data.type === Hls.ErrorTypes.NETWORK_ERROR) refreshVideo();
      else if (data.type === Hls.ErrorTypes.MEDIA_ERROR) this.hls.recoverMediaError();
      else this.destroy();
    });
    this.hls.loadSource(url);
    this.hls.attachMedia(v);
    this.play();
  },

  play() {
    const v = $("video");
    if (!v.paused) return;
    // Sin un gesto del usuario, móviles y Safari solo reproducen sin sonido.
    v.play().catch(() => {
      if (v.muted) return;
      v.muted = true;
      sysLine("El navegador empezó el vídeo sin sonido: haz clic en el vídeo para activarlo. Doble clic, pantalla completa.");
      return v.play().catch(() => {});
    });
  },

  destroy() {
    this.url = null;
    if (this.hls) { this.hls.destroy(); this.hls = null; }
    const v = $("video");
    v.onerror = null;
    v.removeAttribute("src");
    v.load();
  },
};

// hls.js solo se descarga donde hace falta: Safari no lo necesita.
let hlsPromise = null;
function loadHls() {
  if (window.Hls) return Promise.resolve(window.Hls);
  if (!hlsPromise) {
    hlsPromise = new Promise((resolve) => {
      const s = document.createElement("script");
      s.src = "vendor/hls.min.js";
      s.onload = () => resolve(window.Hls);
      s.onerror = () => { hlsPromise = null; resolve(null); };
      document.head.append(s);
    });
  }
  return hlsPromise;
}

// ---------- Acceso al vídeo ----------
// La playlist pide un token con caducidad; el ticket de un solo uso del chat no
// sirve aquí, porque el reproductor pide decenas de ficheros por minuto. El
// origen deja además una cookie atada a la sesión, y es la cookie la que
// autoriza los trozos: sus URLs no llevan nada y las puede cachear un CDN.

let livePath = null;    // /hls/<sesión>/live.m3u8, lo anuncia el repetidor
let videoToken = null;
let videoExp = 0;       // caducidad del token, en segundos desde epoch
let videoTimer = null;

// Token vigente, pidiendo uno nuevo si el que hay está por caducar.
async function videoAccess() {
  if (!authUrl) return null;
  if (videoToken && videoExp - 60 > Date.now() / 1000) return videoToken;
  const t = await authApi("/api/ticket", "POST");
  videoToken = t.video || null;
  videoExp = t.video_exp || 0;
  return videoToken;
}

// Arranca el reproductor con la señal que anuncia el repetidor.
async function startVideo() {
  if (mode !== "viewer" || !livePath) return;
  const path = livePath;
  let token;
  try { token = await videoAccess(); }
  catch (e) {
    if (e instanceof AuthError) { authNeeded(e); return; }
    setPlaceholder("No se pudo pedir permiso para el vídeo. Reintentando…");
    clearTimeout(videoTimer);
    videoTimer = setTimeout(() => livePath === path && startVideo(), 3000);
    return;
  }
  // Mientras se pedía el token pudo empezar otra señal o irse el espectador.
  if (livePath !== path || mode !== "viewer") return;
  setPlaceholder("");
  player.start(location.origin + path + (token ? `?t=${encodeURIComponent(token)}` : ""));
  networkMonitor.start();
  // Se renueva antes de que caduque: si no, el reproductor se queda sin poder
  // pedir la playlist y el directo se corta.
  clearTimeout(videoTimer);
  if (videoExp) {
    videoTimer = setTimeout(refreshVideo, Math.max(5000, (videoExp - 60) * 1000 - Date.now()));
  }
}

// Vuelve a empezar con un token nuevo. Lo llama el reloj de renovación y
// también un error de red del reproductor, que casi siempre es el token
// caducado. Con un espaciado mínimo: si el repetidor está caído, los errores
// llegan en ráfaga y sin esto se pediría un token tras otro.
let lastRefresh = 0;
function refreshVideo() {
  if (mode !== "viewer" || !livePath) return;
  const wait = 3000 - (Date.now() - lastRefresh);
  clearTimeout(videoTimer);
  if (wait > 0) {
    videoTimer = setTimeout(refreshVideo, wait);
    return;
  }
  lastRefresh = Date.now();
  videoToken = null;
  videoExp = 0;
  player.destroy();
  startVideo();
}

// ---------- Calidad de conexión del espectador ----------
// La medida sale del propio elemento de vídeo: cuánto tiene por delante en el
// búfer y cuántas veces se le ha cortado. Con HLS no se cuentan bytes a mano,
// porque los trozos los descarga el reproductor (o Safari) y pueden venir de
// la caché del navegador o del CDN sin pasar por aquí.

const networkMonitor = {
  poorReported: false,
  stalls: 0,
  lastStall: 0,
  timer: null,

  start() {
    this.stop();
    this.poorReported = false;
    this.stalls = 0;
    this.lastStall = 0;
    this.timer = setInterval(() => this.update(), 2000);
  },

  stop() {
    clearInterval(this.timer);
    this.timer = null;
    // Quien deja de ver no debe seguir contando como «va mal».
    if (this.poorReported) {
      this.poorReported = false;
      this.report(0, false);
    }
  },

  // El vídeo se quedó sin datos.
  onStall() {
    const now = Date.now();
    // Dos cortes muy seguidos son el mismo bache.
    if (now - this.lastStall < 1500) return;
    this.lastStall = now;
    this.stalls++;
  },

  // Segundos de vídeo por delante del punto de reproducción.
  ahead() {
    const v = $("video");
    for (let i = 0; i < v.buffered.length; i++) {
      if (v.currentTime >= v.buffered.start(i) && v.currentTime <= v.buffered.end(i)) {
        return v.buffered.end(i) - v.currentTime;
      }
    }
    return 0;
  },

  // Ancho de banda estimado, en kbps. hls.js lo mide al descargar; Safari no
  // lo expone, así que ahí no hay cifra que dar.
  bandwidth() {
    const bps = player.hls && player.hls.bandwidthEstimate;
    return bps > 0 ? bps / 1000 : 0;
  },

  level() {
    const ahead = this.ahead();
    const reciente = this.lastStall && Date.now() - this.lastStall < 20000;
    // Lo que de verdad indica un problema es quedarse sin vídeo por delante.
    if (reciente || ahead < 0.5) return "poor";
    if (ahead < 1.5) return "fair";
    return "good";
  },

  update() {
    if (mode !== "viewer" || !player.url) return;
    const level = this.level();
    const indicator = $("quality-indicator");
    if (indicator) {
      // Solo se avisa de lo que el espectador puede entender como problema. Con
      // buena conexión no se dice nada: no hay nada que hacer al respecto.
      const labels = { poor: "⚠️ Conexión lenta", fair: "⚡ Conexión media" };
      const label = labels[level] || "";
      indicator.textContent = label;
      indicator.className = label ? `quality-indicator quality-${level}` : "quality-indicator";
      indicator.classList.toggle("hidden", !label);
      const bw = Math.round(this.bandwidth());
      indicator.title = (bw > 0 ? `Ancho de banda estimado: ${bw} kbps\n` : "")
        + `Vídeo por delante: ${this.ahead().toFixed(2)}s\nCortes: ${this.stalls}`;
    }
    // Al streamer solo le interesa cuántos espectadores van mal ahora, así que
    // se informa al entrar y al salir del estado malo, no en cada medición.
    const poor = level === "poor";
    if (poor !== this.poorReported) {
      this.poorReported = poor;
      this.report(Math.floor(this.bandwidth()), poor);
    }
  },

  report(bandwidth, poor) {
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ t: "quality_hint", bandwidth, poor }));
    }
  },
};

// ---------- Detección de cortes ----------

for (const ev of ["waiting", "stalled"]) {
  $("video").addEventListener(ev, () => {
    if (mode === "viewer" && player.url) networkMonitor.onStall();
  });
}

// ---------- Gestos sobre el vídeo ----------
// Sin controles visibles: encima del vídeo no se dibuja nada. Un clic alterna
// el sonido, dos abren la pantalla completa.

// Un doble clic dispara también dos "click", así que el sonido se alterna con
// un pequeño retraso que el segundo clic cancela. Si no, pedir pantalla
// completa alternaría el sonido dos veces de paso.
const DBLCLICK_MS = 250;
let clickTimer = null;

$("video").addEventListener("click", () => {
  clearTimeout(clickTimer);
  clickTimer = setTimeout(() => {
    const v = $("video");
    v.muted = !v.muted;
    // Salir de silencio con el volumen a cero no se oiría.
    if (!v.muted && v.volume === 0) v.volume = 1;
    // Los navegadores no reproducen con sonido sin un gesto del usuario: este
    // clic lo es, así que es el momento de reintentarlo.
    if (!v.muted && v.paused) v.play().catch(() => {});
  }, DBLCLICK_MS);
});

$("video").addEventListener("dblclick", () => {
  clearTimeout(clickTimer);
  const p = document.querySelector(".player");
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else if (p.requestFullscreen) p.requestFullscreen().catch(() => {});
  // iPhone no permite pantalla completa en un div, solo en el propio vídeo.
  else if ($("video").webkitEnterFullscreen) $("video").webkitEnterFullscreen();
});

// `hls` es la ruta de la playlist de la señal actual, que el repetidor manda
// en el «welcome» y en cada «live». Cambia con cada señal nueva, así que nada
// cacheado de un directo anterior se puede confundir con el de ahora.
function setLive(live, hlsPath) {
  $("badge").textContent = live ? "EN VIVO" : "OFFLINE";
  $("badge").classList.toggle("on", live);
  setPlaceholder(live ? "" : "Esperando señal…");

  // El indicador de calidad es solo para espectadores, y únicamente cuando hay
  // algo que avisar: lo gestiona networkMonitor.update().
  if (!(mode === "viewer" && live)) $("quality-indicator").classList.add("hidden");

  if (mode === "host") return;
  if (live && hlsPath) {
    livePath = hlsPath;
    startVideo();
  }
  if (!live) {
    livePath = null;
    networkMonitor.stop();
    player.destroy();
  }
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
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    switch (msg.t) {
      case "welcome":
        $("messages").replaceChildren();
        msg.history.forEach(addLine);
        sysLine("Conectado al chat. Los mensajes se borran al terminar la sesión.");
        setViewers(msg.viewers);
        setLive(msg.live, msg.hls);
        break;
      case "chat": addLine(msg.m); break;
      case "viewers": setViewers(msg.n); break;
      case "live": setLive(true, msg.hls); break;
      case "offline": setLive(false); break;
      case "clear": $("messages").replaceChildren(); sysLine("El streamer vació el chat."); break;
      case "end": sysLine("La sesión ha terminado."); setLive(false); wsUrl = null; break;
      case "auth": onAuthRequired(msg); break;
    }
  };
  ws.onclose = (ev) => {
    if (!wsUrl || ev.target !== ws) return;
    // El vídeo va por HTTP y no depende de esta conexión: perder el chat no
    // es motivo para cortarle el directo a nadie.
    sysLine("Chat desconectado, reintentando…");
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
  livePath = null;
  videoToken = null;
  videoExp = 0;
  clearTimeout(videoTimer);
  networkMonitor.stop();
  player.destroy();
  $("messages").replaceChildren();
  if (mode === "host") await invoke("stop_stream").catch(() => {});
  mode = null;
  $("live").classList.remove("host");
  $("leave").classList.add("hidden");
  show("host-setup");
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

  // Espectadores con problemas de conexión ahora mismo. La píldora solo
  // aparece si hay alguno: «todo bien» no es información que requiera sitio.
  const viewers = (st.viewers || 0) + (st.relay_viewers || 0);
  const poor = st.quality_poor_now || 0;
  if (st.live && poor > 0 && viewers > 0) {
    const pct = Math.round((poor / viewers) * 100);
    const avgBw = Math.round(st.quality_avg_bandwidth);
    const tooltip = `${poor} de ${viewers} espectadores con la conexión justa.\n` +
      (avgBw > 0 ? `Ancho de banda medio de quien informa: ${avgBw} kbps.\n` : "") +
      "Si el porcentaje se mantiene alto, baja el bitrate en OBS.";
    pill("h-quality", pct > 30 ? "bad" : "warn", `${pct}% con lag`, tooltip);
    $("h-quality").classList.remove("hidden");
  } else {
    $("h-quality").classList.add("hidden");
  }

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
    $("leave").classList.remove("hidden");
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

// ---------- Espectador (web) ----------
// Se entra directo al directo: el nombre sale de la cuenta de Discord y el
// repetidor pide iniciar sesión si hace falta.

function watch() {
  mode = "viewer";
  show("live");
  viewerBase = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?v=1`;
  // El servicio de cuentas está en el mismo dominio que la web.
  authUrl = location.origin;
  connect(viewerUrl);
}

// ---------- Cuentas de Discord y suscripción ----------
// Si el repetidor exige cuenta, responde {t:"auth", url} y cierra. La app
// inicia sesión en esa dirección (navegador del sistema), guarda el token de
// sesión y pide un ticket de un solo uso para cada conexión.

class AuthError extends Error {
  constructor(code, invite) { super(code); this.code = code; this.invite = invite; }
}

let viewerBase = null;   // wss://…/ws?…
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
  // Una misma llamada da las dos cosas: el ticket de un solo uso para el chat
  // y el token con caducidad para el vídeo.
  const [t, me] = await Promise.all([authApi("/api/ticket", "POST"), authApi("/api/me")]);
  videoToken = t.video || null;
  videoExp = t.video_exp || 0;
  account = me;
  renderAccount();
  return `${viewerBase}&t=${t.ticket}`;
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
  setPlaceholder("");
  $("login-panel").classList.remove("hidden");
  $("login-msg").textContent = e.code === "not_member"
    ? "Para ver este stream tienes que estar en el servidor de Discord. Únete y vuelve a iniciar sesión."
    : "Este stream es solo para miembros del servidor de Discord.";
  if (loginError) { $("login-msg").textContent = loginError; loginError = null; }
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

let loginError = null;

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
  catch (e) { loginError = String(e.message || e); }
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
  $("k-stream").textContent = k.stream_key;
  $("k-rtmp").textContent = `rtmp://127.0.0.1:${Number($("rtmp-port").value)}/live`;
  // La web vive en el VPS, con el certificado de <ip-con-guiones>.sslip.io.
  const host = $("vps").value.trim().split("@").pop();
  const web = /^\d+\.\d+\.\d+\.\d+$/.test(host) ? `${host.replace(/\./g, "-")}.sslip.io` : host;
  $("k-web").textContent = web ? `https://${web}/` : "Indica el VPS";
}

for (const id of ["vps", "ws-port", "rtmp-port"])
  $(id).addEventListener("input", () => keys && showKeys(keys));

async function regen(which, msg) {
  if (!confirm(msg)) return;
  try { showKeys(await invoke("regenerate_keys", { which })); }
  catch (e) { $("host-error").textContent = String(e); }
}

$("regen-stream").addEventListener("click", () =>
  regen("stream", "¿Generar una clave de stream nueva? Tendrás que pegarla otra vez en OBS."));

$("leave").addEventListener("click", leave);

document.querySelectorAll("[data-copy]").forEach((b) =>
  b.addEventListener("click", () => navigator.clipboard.writeText($(b.dataset.copy).textContent)));
$("vps").value = localStorage.getItem("vps") ?? "stream@109.123.252.126";
$("vps-key").value = localStorage.getItem("vps-key") ?? "~/.ssh/mestisexo_tunnel";

if (native) {
  show("host-setup");
  invoke("get_keys").then(showKeys, (e) => { $("host-error").textContent = String(e); });
} else {
  document.body.classList.add("web");
  localStorage.removeItem("web-key");
  finishWebLogin().then(watch);
}
