import type Hls from "hls.js";

// ---------- Reproductor (LL-HLS) ----------
// El vídeo llega por HTTP en trozos que cualquier CDN puede cachear, así que
// aquí no hay control de búfer propio: la latencia la gobierna la playlist
// (PART-HOLD-BACK) y la gestiona hls.js, o Safari por su cuenta.

type HlsClass = typeof Hls;

// hls.js va en un trozo aparte y solo se descarga donde hace falta: Safari no
// lo necesita.
let hlsPromise: Promise<HlsClass | null> | null = null;
function loadHls(): Promise<HlsClass | null> {
  hlsPromise ??= import("hls.js").then(
    (m) => m.default,
    () => {
      hlsPromise = null;
      return null;
    },
  );
  return hlsPromise;
}

export interface PlayerHooks {
  /** Error de red o del elemento de vídeo: casi siempre el token caducado o un corte. */
  refresh(): void;
  unsupported(): void;
  /** El navegador solo dejó empezar sin sonido. */
  mutedAutoplay(): void;
}

export class Player {
  hls: Hls | null = null;
  url: string | null = null;
  video: HTMLVideoElement | null = null;
  #hooks: PlayerHooks;

  constructor(hooks: PlayerHooks) {
    this.#hooks = hooks;
  }

  /** `url` es la playlist, ya con el token de acceso. */
  async start(url: string) {
    const v = this.video;
    if (!v || this.url === url) return;
    this.destroy();
    this.url = url;
    // Safari reproduce HLS por sí mismo y lo hace mejor que cualquier
    // biblioteca: menos batería en iPhone y pantalla completa nativa.
    if (v.canPlayType("application/vnd.apple.mpegurl")) {
      v.src = url;
      // Safari no avisa de por qué falla: si el token caducó, lo único visible
      // es un error del elemento de vídeo, así que se renueva y se reintenta.
      v.onerror = () => this.#hooks.refresh();
      this.play();
      return;
    }
    const H = await loadHls();
    // Mientras se descargaba hls.js pudo pararse o cambiar la señal.
    if (this.url !== url) return;
    if (!H || !H.isSupported()) {
      this.#hooks.unsupported();
      return;
    }
    const hls = new H({
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
    this.hls = hls;
    hls.on(H.Events.ERROR, (_, data) => {
      if (!data.fatal) return;
      if (data.type === H.ErrorTypes.NETWORK_ERROR) this.#hooks.refresh();
      else if (data.type === H.ErrorTypes.MEDIA_ERROR) hls.recoverMediaError();
      else this.destroy();
    });
    hls.loadSource(url);
    hls.attachMedia(v);
    this.play();
  }

  play() {
    const v = this.video;
    if (!v || !v.paused) return;
    // Sin un gesto del usuario, móviles y Safari solo reproducen sin sonido.
    v.play().catch(() => {
      if (v.muted) return;
      v.muted = true;
      this.#hooks.mutedAutoplay();
      return v.play().catch(() => {});
    });
  }

  /** Ancho de banda estimado, en kbps. hls.js lo mide al descargar; Safari no lo expone. */
  bandwidth(): number {
    const bps = this.hls?.bandwidthEstimate ?? 0;
    return bps > 0 ? bps / 1000 : 0;
  }

  destroy() {
    this.url = null;
    if (this.hls) {
      this.hls.destroy();
      this.hls = null;
    }
    const v = this.video;
    if (!v) return;
    v.onerror = null;
    v.removeAttribute("src");
    v.load();
  }
}
