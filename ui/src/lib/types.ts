// Lo que manda el repetidor (o el streamer local) por el WebSocket del chat.
// Debe coincidir con los `json!({ "t": … })` de core/src.

export interface ChatMsg {
  id: number;
  name: string;
  text: string;
  ts: number;
  host: boolean;
  sub?: boolean;
}

export type ServerMsg =
  | { t: "welcome"; history: ChatMsg[]; live: boolean; hls?: string | null; viewers: number; host?: boolean }
  | { t: "chat"; m: ChatMsg }
  | { t: "viewers"; n: number }
  | { t: "live"; hls?: string | null }
  | { t: "offline" }
  | { t: "clear" }
  | { t: "end" }
  | { t: "auth"; url: string; retry?: boolean };

export type ClientMsg =
  | { t: "chat"; text: string }
  | { t: "quality_hint"; bandwidth: number; poor: boolean };

// Servicio de cuentas (auth/src/api.rs).
export interface Me {
  id?: string;
  name: string;
  sub: boolean;
  payments: boolean;
}

export interface Ticket {
  ticket: string;
  video?: string | null;
  video_exp?: number;
}

// Comandos de Tauri (app/src/main.rs).
export interface Keys {
  viewer_key: string;
  stream_key: string;
}

export interface HostSession {
  rtmp_server: string;
  stream_key: string;
  ws_port: number;
  key: string;
  host_token: string;
}
