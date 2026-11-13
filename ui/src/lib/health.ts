import type { Status } from "./types";

// Estado de la sesión del streamer, en píldoras para la cabecera.

export interface Pill {
  cls: string;
  text: string;
  title?: string;
}

export interface Health {
  obs: Pill;
  rate: Pill;
  tunnel: Pill | null;
  quality: Pill | null;
  uptime: string | null;
}

export function fmtTime(ms: number): string {
  const t = Math.floor(ms / 1000), h = Math.floor(t / 3600), m = Math.floor(t / 60) % 60, s = t % 60;
  return (h ? `${h}:` : "") + `${String(m).padStart(h ? 2 : 1, "0")}:${String(s).padStart(2, "0")}`;
}

export function health(st: Status, prev: Status | null): Health {
  const stale = st.live && st.now_ms - st.last_data_ms > 3000;
  const obs: Pill = {
    cls: st.live ? (stale ? "bad" : "ok") : "warn",
    text: st.live ? (stale ? "OBS sin datos" : "OBS conectado") : "Esperando a OBS",
    title: st.status,
  };

  let kbps = 0;
  if (prev && st.now_ms > prev.now_ms && st.bytes_total >= prev.bytes_total)
    kbps = ((st.bytes_total - prev.bytes_total) * 8) / (st.now_ms - prev.now_ms);
  const rate: Pill = {
    cls: !st.live ? "" : kbps > 0 ? "ok" : "warn",
    text: st.live ? `${Math.round(kbps)} kbps` : "— kbps",
  };

  const tunnel: Pill | null = st.tunnel_up === null || st.tunnel_up === undefined
    ? null
    : { cls: st.tunnel_up ? "ok" : "bad", text: st.tunnel_up ? "VPS conectado" : "VPS caído", title: st.tunnel ?? "" };

  // Espectadores con problemas de conexión ahora mismo. La píldora solo
  // aparece si hay alguno: «todo bien» no es información que requiera sitio.
  const viewers = (st.viewers || 0) + (st.relay_viewers || 0);
  const poor = st.quality_poor_now || 0;
  let quality: Pill | null = null;
  if (st.live && poor > 0 && viewers > 0) {
    const pct = Math.round((poor / viewers) * 100);
    const avgBw = Math.round(st.quality_avg_bandwidth);
    quality = {
      cls: pct > 30 ? "bad" : "warn",
      text: `${pct}% con lag`,
      title: `${poor} de ${viewers} espectadores con la conexión justa.\n` +
        (avgBw > 0 ? `Ancho de banda medio de quien informa: ${avgBw} kbps.\n` : "") +
        "Si el porcentaje se mantiene alto, baja el bitrate en OBS.",
    };
  }

  return { obs, rate, tunnel, quality, uptime: st.live_since ? fmtTime(st.now_ms - st.live_since) : null };
}

/** El enlace de los espectadores: la web vive en el VPS, con el certificado de <ip-con-guiones>.sslip.io. */
export function webLink(vps: string): string | null {
  const host = vps.trim().split("@").pop() ?? "";
  const web = /^\d+\.\d+\.\d+\.\d+$/.test(host) ? `${host.replace(/\./g, "-")}.sslip.io` : host;
  return web ? `https://${web}/` : null;
}
