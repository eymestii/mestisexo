// La app del streamer corre dentro de Tauri (con `withGlobalTauri`); la web de
// los espectadores, en un navegador normal. Se usa el objeto global en vez de
// @tauri-apps/api para no añadir nada al paquete de la web.

declare global {
  interface Window {
    __TAURI__?: {
      core: { invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> };
    };
  }
}

export const native = !!window.__TAURI__;

export function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const tauri = window.__TAURI__;
  return tauri ? tauri.core.invoke<T>(cmd, args) : Promise.reject(new Error("No disponible en la web"));
}
