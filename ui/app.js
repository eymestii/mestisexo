"use strict";

// La misma interfaz sirve para la app del streamer (Tauri) y para la web de
// los espectadores, que se conecta al repetidor del VPS en /ws.
const native = !!window.__TAURI__;
const invoke = native ? window.__TAURI__.core.invoke : () => Promise.reject(new Error("No disponible en la web"));
const $ = (id) => document.getElementById(id);
const MAX_LINES = 200;
// Segundos por detrás del directo. El objetivo es adaptativo: cada espectador
// usa el margen que su red necesita (ver liveTarget). Quien tiene buena
// conexión se queda en TARGET_MIN; a quien se le corta el vídeo se le sube el
// margen, porque más retraso molesta mucho menos que un corte.
const TARGET_MIN = 0.5;
const TARGET_MAX = 3.0;
// El salto al directo se mide sobre el objetivo de cada uno, no en absoluto.
const JUMP_OVER = 1.0;
// Recuperación del retraso. Cambiar playbackRate obliga al navegador a
// reprocesar el audio, y hacerlo a menudo lo corta, así que: un solo escalón
// suave y con histéresis (se entra tarde y se sale pronto) para que la
// velocidad no oscile. CATCH_IN > CATCH_OUT a propósito.
const CATCH_RATE = 1.05;
const CATCH_IN = 0.4;   // retraso sobre el objetivo al que se empieza a acelerar (0,9 s detrás)
const CATCH_OUT = 0.15; // retraso al que se vuelve a velocidad normal
const CATCH_HOLD = 2000; // ms mínimos entre cambios de velocidad

let ws = null;
let mode = null;            // "host" | "viewer"
let statusTimer = null;
let reconnectTimer = null;
let wsUrl = null;

function show(id) {
  for (const s of ["host-setup", "live"]) $(s).classList.toggle("hidden", s !== id);
}

// ---------- Monitor de Red ----------

const networkMonitor = {
  samples: [],
  maxSamples: 20,
  bytesReceived: 0,
  lastMeasure: Date.now(),
  poorReported: false,

  recordBytes(bytes) {
    this.bytesReceived += bytes;
    const now = Date.now();
    const elapsed = now - this.lastMeasure;
    
    if (elapsed > 2000) {
      const kbps = (this.bytesReceived * 8) / elapsed;
      this.samples.push(kbps);
      if (this.samples.length > this.maxSamples) this.samples.shift();
      
      this.bytesReceived = 0;
      this.lastMeasure = now;
      this.updateQualityUI();
      return kbps;
    }
    return null;
  },

  getAverageBandwidth() {
    if (this.samples.length === 0) return 0;
    const sum = this.samples.reduce((a, b) => a + b, 0);
    return sum / this.samples.length;
  },

  getBufferHealth() {
    const v = $("video");
    const b = v.buffered;
    if (b.length === 0) return 0;
    const end = b.end(b.length - 1);
    return end - v.currentTime;
  },

  checkQuality() {
    const bandwidth = this.getAverageBandwidth();
    const bufferHealth = this.getBufferHealth();
    
    if (this.samples.length < 5) return { level: "measuring", bandwidth, bufferHealth };
    
    // Estimar bitrate necesario (con margen del 30%)
    const estimatedBitrate = bandwidth / 1.3;
    // La salud del búfer se juzga contra el margen de este espectador: con un
    // objetivo de 3 s, medio segundo de búfer es poco, aunque con 0,5 s no.
    const health = bufferHealth / liveTarget.value;
    
    if (health < 0.6 || bandwidth < 1000) {
      return { level: "poor", bandwidth, bufferHealth, estimatedBitrate };
    } else if (health < 0.9 || bandwidth < 1500) {
      return { level: "fair", bandwidth, bufferHealth, estimatedBitrate };
    } else if (bandwidth < 3000) {
      return { level: "good", bandwidth, bufferHealth, estimatedBitrate };
    } else {
      return { level: "excellent", bandwidth, bufferHealth, estimatedBitrate };
    }
  },

  updateQualityUI() {
    const quality = this.checkQuality();
    const indicator = $("quality-indicator");
    if (!indicator) return;

    // Solo se avisa de lo que el espectador puede entender como problema. Con
    // buena conexión no se dice nada: no hay nada que hacer al respecto.
    const labels = { poor: "⚠️ Conexión lenta", fair: "⚡ Conexión media" };
    const label = labels[quality.level] || "";
    indicator.textContent = label;
    indicator.className = label ? `quality-indicator quality-${quality.level}` : "quality-indicator";
    indicator.classList.toggle("hidden", !label);

    const bw = Math.round(quality.bandwidth);
    const buf = quality.bufferHealth.toFixed(2);
    indicator.title = `Ancho de banda: ${bw} kbps\nBuffer: ${buf}s de ${liveTarget.value.toFixed(1)}s de margen`;

    // Al streamer solo le interesa cuántos espectadores van mal ahora, así que
    // se informa al entrar y al salir del estado malo, no en cada medición.
    const poor = quality.level === "poor";
    if (poor !== this.poorReported && ws && ws.readyState === WebSocket.OPEN) {
      this.poorReported = poor;
      ws.send(JSON.stringify({ t: "quality_hint", bandwidth: Math.floor(quality.bandwidth), poor }));
    }
  },

  reset() {
    this.samples = [];
    this.bytesReceived = 0;
    this.lastMeasure = Date.now();
    this.poorReported = false;
  }
};

// ---------- Objetivo de búfer adaptativo ----------
// La misma idea que usan las plataformas grandes: el reproductor va un poco por
// detrás del directo para absorber los baches de la red. Cuánto, depende de la
// red de cada espectador, y aquí se mide por sus cortes reales: cada vez que el
// vídeo se queda sin datos, el margen sube; si aguanta mucho tiempo sin
// cortarse, baja despacio. Sube rápido y baja lento a propósito: al contrario
// oscilaría, y oscilar es justo lo que rompe el audio.

const liveTarget = {
  value: TARGET_MIN,
  stalls: 0,
  lastStall: 0,
  lastDrop: 0,
  warned: false,
  // Cuánto se sube por corte y cuánto se baja al ir bien.
  STEP_UP: 0.5,
  STEP_DOWN: 0.25,
  // Un corte reabre el margen; dos seguidos muy juntos no cuentan doble.
  STALL_IGNORE: 1500,
  // Para bajar hacen falta 60 s sin un solo corte, y se baja cada 30 s.
  CALM_MS: 60000,
  DROP_EVERY: 30000,
  KEY: "mestisexo.liveTarget",

  // El margen aprendido se recuerda: el espectador con mala red arranca ya con
  // holgura en vez de descubrirla a base de cortes.
  load() {
    this.stalls = 0;
    this.warned = false;
    this.lastStall = Date.now();
    this.lastDrop = Date.now();
    let guardado = NaN;
    try { guardado = parseFloat(localStorage.getItem(this.KEY)); } catch { /* modo privado */ }
    this.value = guardado >= TARGET_MIN && guardado <= TARGET_MAX ? guardado : TARGET_MIN;
  },

  save() {
    try { localStorage.setItem(this.KEY, this.value.toFixed(2)); } catch { /* modo privado */ }
  },

  // El vídeo se quedó sin datos: el margen era insuficiente.
  onStall() {
    const now = Date.now();
    if (now - this.lastStall < this.STALL_IGNORE) return;
    this.lastStall = now;
    this.lastDrop = now;
    this.stalls++;
    const antes = this.value;
    this.value = Math.min(TARGET_MAX, this.value + this.STEP_UP);
    if (this.value === antes) return;
    this.save();
    // Un solo aviso por sesión: lo que importa es que deje de cortarse, no
    // llenar el chat de mensajes cada vez que se ajusta el margen.
    if (!this.warned) {
      this.warned = true;
      sysLine("Tu conexión perdía el vídeo: se aumentó el margen del reproductor para que no se corte. Irás unos segundos por detrás del directo.");
    }
  },

  // Sin cortes durante un buen rato: se recupera cercanía al directo, despacio.
  maybeRelax() {
    if (this.value <= TARGET_MIN) return;
    const now = Date.now();
    if (now - this.lastStall < this.CALM_MS) return;
    if (now - this.lastDrop < this.DROP_EVERY) return;
    this.lastDrop = now;
    this.value = Math.max(TARGET_MIN, this.value - this.STEP_DOWN);
    this.save();
  }
};

// ---------- Reproductor (MediaSource con MP4 fragmentado) ----------

const player = {
  ms: null, sb: null, queue: [], codec: null, waitKey: false, timer: null,
  droppedFrames: 0, totalFrames: 0, catching: false, rateAt: 0,

  reset(codec) {
    this.destroy();
    this.codec = codec;
    this.droppedFrames = 0;
    this.totalFrames = 0;
    this.catching = false;
    this.rateAt = 0;
    liveTarget.load();
    networkMonitor.reset();
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
    
    // Registrar bytes para el monitor de red
    if (kind !== 0 && mode === "viewer") {
      networkMonitor.recordBytes(buf.byteLength);
    }
    
    this.totalFrames++;
    
    if (kind === 0) { this.waitKey = false; this.queue.push(buf); this.flush(); return; }
    // Si se han perdido fragmentos, los siguientes no se pueden decodificar
    // hasta el próximo keyframe.
    if (this.waitKey && kind !== 1) {
      this.droppedFrames++;
      return;
    }
    this.waitKey = false;
    this.queue.push(buf);
    if (this.queue.length > 8) { 
      this.droppedFrames += this.queue.length;
      this.queue = []; 
      this.waitKey = true; 
    }
    this.flush();
  },

  flush() {
    const sb = this.sb, v = $("video");
    if (!sb || sb.updating || this.ms.readyState !== "open") return;
    // Sin cortes durante un buen rato, el margen se reduce para volver a estar
    // cerca del directo. Aquí porque flush() corre cada 250 ms pase lo que pase.
    liveTarget.maybeRelax();
    // Ir por detrás del directo lo que la red de este espectador necesite.
    const target = liveTarget.value;
    const b = sb.buffered;
    if (b.length) {
      const start = b.start(0), end = b.end(b.length - 1), behind = end - v.currentTime;
      if (behind > target + JUMP_OVER) v.currentTime = end - target;
      else {
        // Saltar huecos: si el tiempo actual no cae dentro de ningún rango con
        // datos, el vídeo se quedaría congelado esperando algo que no va a llegar.
        for (let i = 0; i < b.length; i++) {
          if (v.currentTime >= b.start(i) && v.currentTime < b.end(i)) break;
          if (v.currentTime < b.start(i)) { v.currentTime = b.start(i); break; }
        }
      }
      // Recuperar el retraso con un único escalón suave. Dos frenos para que la
      // velocidad no oscile: histéresis (se entra en CATCH_IN y no se sale
      // hasta CATCH_OUT) y un mínimo de CATCH_HOLD entre cambios. Cada cambio
      // obliga al navegador a reprocesar el audio, y hacerlo seguido lo corta.
      const delay = behind - target;
      const want = this.catching ? delay >= CATCH_OUT : delay > CATCH_IN;
      if (want !== this.catching && Date.now() - this.rateAt > CATCH_HOLD) {
        this.catching = want;
        this.rateAt = Date.now();
        v.playbackRate = want ? CATCH_RATE : 1;
      }
      // Con el margen holgado hay que guardar más de lo ya visto para poder
      // mantenerlo, así que la ventana se mide sobre el objetivo.
      const keep = Math.max(8, target + 5);
      if (v.currentTime - start > keep) { sb.remove(start, v.currentTime - target - 2); return; }
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
        sysLine("El navegador empezó el vídeo sin sonido: haz clic en el vídeo para activarlo. Doble clic, pantalla completa.");
        return v.play().catch(() => {});
      }).finally(() => { this.starting = false; });
    }
  },

  destroy() {
    clearInterval(this.timer); this.timer = null;
    this.queue = []; this.sb = null; this.codec = null; this.waitKey = false;
    this.droppedFrames = 0; this.totalFrames = 0; this.catching = false; this.rateAt = 0;
    const v = $("video");
    if (this.ms) { v.removeAttribute("src"); v.load(); this.ms = null; }
    if (this.url) { URL.revokeObjectURL(this.url); this.url = null; }
    v.playbackRate = 1;
  },

  getStats() {
    return {
      dropped: this.droppedFrames,
      total: this.totalFrames,
      dropRate: this.totalFrames > 0 ? (this.droppedFrames / this.totalFrames * 100).toFixed(1) : 0
    };
  }
};

// ---------- Detección de cortes ----------
// "waiting" es el navegador diciendo que se quedó sin datos para seguir: es la
// señal exacta de que el margen de este espectador no daba. Cada corte sube el
// objetivo; aguantar sin cortes lo baja despacio (ver liveTarget).

$("video").addEventListener("waiting", () => {
  if (mode === "viewer" && player.ms) liveTarget.onStall();
});

$("video").addEventListener("stalled", () => {
  if (mode === "viewer" && player.ms) liveTarget.onStall();
});

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

function setLive(live, codec) {
  $("badge").textContent = live ? "EN VIVO" : "OFFLINE";
  $("badge").classList.toggle("on", live);
  setPlaceholder(live ? "" : "Esperando señal…");
  
  // El indicador de calidad es solo para los espectadores.
  // El indicador de calidad es solo para espectadores, y únicamente cuando hay
  // algo que avisar: lo gestiona networkMonitor.updateQualityUI().
  if (!(mode === "viewer" && live)) $("quality-indicator").classList.add("hidden");

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
