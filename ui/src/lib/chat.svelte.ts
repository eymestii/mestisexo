import type { ChatMsg, ClientMsg, ServerMsg } from "./types";

export const MAX_LINES = 200;

export type Line =
  | { id: number; sys: true; text: string }
  | { id: number; sys: false; m: ChatMsg };

export interface ChatHooks {
  /** Empieza o termina el directo. `hls` es la ruta de la playlist de la señal actual. */
  onLive(live: boolean, hls?: string | null): void;
  /** El repetidor exige cuenta de Discord. */
  onAuth?(msg: Extract<ServerMsg, { t: "auth" }>): void;
  /** Falló al calcular la dirección. Devuelve `true` si ya lo ha gestionado. */
  onUrlError?(e: unknown): boolean;
}

/** La dirección puede ser una función asíncrona: con cuentas de Discord cada conexión necesita un ticket nuevo. */
export type ChatUrl = string | (() => Promise<string>);

export function colorFor(name: string): string {
  let h = 0;
  for (const c of name) h = (h * 31 + c.codePointAt(0)!) >>> 0;
  return `hsl(${h % 360} 70% 65%)`;
}

export function viewersLabel(n: number): string {
  return `${n} espectador${n === 1 ? "" : "es"}`;
}

export function errorText(e: unknown): string {
  return String((e as Error)?.message || e);
}

/** Conexión WebSocket del chat, con reconexión. El vídeo no depende de ella. */
export class ChatConnection {
  lines = $state<Line[]>([]);
  viewers = $state(0);
  /** Cambia con cada línea añadida, para saber cuándo bajar el scroll. */
  seq = $state(0);

  #ws: WebSocket | null = null;
  #url: ChatUrl | null = null;
  #reconnect: ReturnType<typeof setTimeout> | undefined;
  #hooks: ChatHooks;

  constructor(hooks: ChatHooks) {
    this.#hooks = hooks;
  }

  add(m: ChatMsg) {
    this.#push({ id: this.seq + 1, sys: false, m });
  }

  sys(text: string) {
    this.#push({ id: this.seq + 1, sys: true, text });
  }

  clear() {
    this.lines = [];
  }

  #push(line: Line) {
    this.seq = line.id;
    this.lines.push(line);
    if (this.lines.length > MAX_LINES) this.lines.splice(0, this.lines.length - MAX_LINES);
  }

  send(msg: ClientMsg): boolean {
    if (!this.#ws || this.#ws.readyState !== WebSocket.OPEN) return false;
    this.#ws.send(JSON.stringify(msg));
    return true;
  }

  async connect(url: ChatUrl) {
    this.#url = url;
    clearTimeout(this.#reconnect);
    let target: string;
    if (typeof url === "function") {
      try {
        target = await url();
      } catch (e) {
        if (this.#url !== url) return;
        if (this.#hooks.onUrlError?.(e)) return;
        this.sys(errorText(e) + " Reintentando…");
        this.#reconnect = setTimeout(() => this.#url === url && this.connect(url), 5000);
        return;
      }
      if (this.#url !== url) return;
    } else {
      target = url;
    }
    const ws = new WebSocket(target);
    this.#ws = ws;
    ws.onmessage = (e) => this.#onMessage(JSON.parse(e.data) as ServerMsg);
    ws.onclose = () => {
      if (!this.#url || ws !== this.#ws) return;
      // El vídeo va por HTTP y no depende de esta conexión: perder el chat no
      // es motivo para cortarle el directo a nadie.
      this.sys("Chat desconectado, reintentando…");
      const again = this.#url;
      this.#reconnect = setTimeout(() => this.#url === again && this.connect(again), 3000);
    };
  }

  #onMessage(msg: ServerMsg) {
    switch (msg.t) {
      case "welcome":
        this.clear();
        msg.history.forEach((m) => this.add(m));
        this.sys("Conectado al chat. Los mensajes se borran al terminar la sesión.");
        this.viewers = msg.viewers;
        this.#hooks.onLive(msg.live, msg.hls);
        break;
      case "chat": this.add(msg.m); break;
      case "viewers": this.viewers = msg.n; break;
      case "live": this.#hooks.onLive(true, msg.hls); break;
      case "offline": this.#hooks.onLive(false); break;
      case "clear": this.clear(); this.sys("El streamer vació el chat."); break;
      case "end": this.sys("La sesión ha terminado."); this.#hooks.onLive(false); this.#url = null; break;
      case "auth": this.#hooks.onAuth?.(msg); break;
    }
  }

  /** Deja de reconectar, pero no cierra la conexión actual. */
  forget() {
    this.#url = null;
  }

  /** Cierra sin reconectar. */
  drop() {
    if (this.#ws) {
      this.#ws.onclose = null;
      this.#ws.close();
    }
  }

  /** Cierra y vuelve a entrar, por ejemplo para estrenar la insignia de suscriptor. */
  reconnect() {
    this.#ws?.close();
  }

  stop() {
    this.#url = null;
    clearTimeout(this.#reconnect);
    this.drop();
    this.#ws = null;
  }
}
