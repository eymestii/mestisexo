// ---------- Calidad de conexión del espectador ----------
// La medida sale del propio elemento de vídeo: cuánto tiene por delante en el
// búfer y cuántas veces se le ha cortado. Con HLS no se cuentan bytes a mano,
// porque los trozos los descarga el reproductor (o Safari) y pueden venir de
// la caché del navegador o del CDN sin pasar por aquí.

export type Level = "good" | "fair" | "poor";

/** Segundos de vídeo por delante del punto de reproducción. */
export function bufferAhead(v: HTMLVideoElement): number {
  for (let i = 0; i < v.buffered.length; i++) {
    if (v.currentTime >= v.buffered.start(i) && v.currentTime <= v.buffered.end(i)) {
      return v.buffered.end(i) - v.currentTime;
    }
  }
  return 0;
}

export function levelFor(ahead: number, lastStall: number, now: number): Level {
  const reciente = lastStall > 0 && now - lastStall < 20000;
  // Lo que de verdad indica un problema es quedarse sin vídeo por delante.
  if (reciente || ahead < 0.5) return "poor";
  if (ahead < 1.5) return "fair";
  return "good";
}

// El espectador no ve el nivel: solo se informa al streamer de cuántos van mal.

export interface MonitorSource {
  video(): HTMLVideoElement | null;
  /** Hay reproducción en marcha que medir. */
  active(): boolean;
  bandwidth(): number;
  report(bandwidth: number, poor: boolean): void;
}

export class NetworkMonitor {
  level = $state<Level>("good");
  title = $state("");

  #poorReported = false;
  #stalls = 0;
  #lastStall = 0;
  #timer: ReturnType<typeof setInterval> | undefined;
  #src: MonitorSource;

  constructor(src: MonitorSource) {
    this.#src = src;
  }

  start() {
    this.stop();
    this.#stalls = 0;
    this.#lastStall = 0;
    this.#timer = setInterval(() => this.update(), 2000);
  }

  stop() {
    clearInterval(this.#timer);
    this.#timer = undefined;
    this.level = "good";
    // Quien deja de ver no debe seguir contando como «va mal».
    if (this.#poorReported) {
      this.#poorReported = false;
      this.#src.report(0, false);
    }
  }

  /** El vídeo se quedó sin datos. */
  onStall() {
    const now = Date.now();
    // Dos cortes muy seguidos son el mismo bache.
    if (now - this.#lastStall < 1500) return;
    this.#lastStall = now;
    this.#stalls++;
  }

  update() {
    const v = this.#src.video();
    if (!v || !this.#src.active()) return;
    const ahead = bufferAhead(v);
    const level = levelFor(ahead, this.#lastStall, Date.now());
    const bw = Math.round(this.#src.bandwidth());
    this.level = level;
    this.title = (bw > 0 ? `Ancho de banda estimado: ${bw} kbps\n` : "")
      + `Vídeo por delante: ${ahead.toFixed(2)}s\nCortes: ${this.#stalls}`;
    // Al streamer solo le interesa cuántos espectadores van mal ahora, así que
    // se informa al entrar y al salir del estado malo, no en cada medición.
    const poor = level === "poor";
    if (poor !== this.#poorReported) {
      this.#poorReported = poor;
      this.#src.report(Math.floor(this.#src.bandwidth()), poor);
    }
  }
}
