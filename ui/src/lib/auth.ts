// Servicio de cuentas: sesión de Discord, tickets y pagos.

export class AuthError extends Error {
  code: "session" | "not_member";
  invite: string | null;
  constructor(code: "session" | "not_member", invite: string | null = null) {
    super(code);
    this.code = code;
    this.invite = invite;
  }
}

const tokenKey = (authUrl: string) => `auth-token:${authUrl}`;

export const getToken = (authUrl: string | null) => (authUrl ? localStorage.getItem(tokenKey(authUrl)) : null);

export const dropToken = (authUrl: string) => localStorage.removeItem(tokenKey(authUrl));

export async function authApi<T>(authUrl: string, path: string, method = "GET"): Promise<T> {
  const token = getToken(authUrl);
  if (!token) throw new AuthError("session");
  let r: Response;
  try {
    r = await fetch(authUrl + path, { method, headers: { Authorization: `Bearer ${token}` } });
  } catch {
    throw new Error("No se pudo contactar con el servicio de cuentas.");
  }
  const body = await r.json().catch(() => ({}));
  if (r.status === 401) {
    dropToken(authUrl);
    throw new AuthError("session");
  }
  if (r.status === 403 && body.error === "not_member") throw new AuthError("not_member", body.invite);
  if (!r.ok) throw new Error(body.error || `Error ${r.status}`);
  return body as T;
}

function randomState(): string {
  const abc = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  return Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => abc[b % abc.length]).join("");
}

/** Sale a Discord, que vuelve a esta misma página con ?code=…&state=…. */
export function startLogin(authUrl: string) {
  const state = randomState();
  localStorage.setItem("login-state", state);
  location.href = `${authUrl}/login?state=${state}`;
}

async function exchangeCode(authUrl: string, code: string) {
  const r = await fetch(`${authUrl}/api/exchange?code=${encodeURIComponent(code)}`, { method: "POST" });
  const body = await r.json().catch(() => ({}));
  if (!r.ok || !body.token) throw new Error("No se pudo completar el inicio de sesión.");
  localStorage.setItem(tokenKey(authUrl), body.token);
}

/**
 * Si la página viene de vuelta de Discord, canjea el código por la sesión. Solo
 * lo acepta si coincide con el `state` guardado antes de salir. Devuelve el
 * error a mostrar, si lo hubo.
 */
export async function finishWebLogin(authUrl: string): Promise<string | null> {
  const q = new URLSearchParams(location.search);
  if (!q.has("code")) return null;
  const expected = localStorage.getItem("login-state");
  localStorage.removeItem("login-state");
  history.replaceState(null, "", location.pathname + location.hash);
  if (!expected || q.get("state") !== expected) return null;
  try {
    await exchangeCode(authUrl, q.get("code")!);
    return null;
  } catch (e) {
    return String((e as Error).message || e);
  }
}

/** En otra pestaña; si el navegador no deja, en esta misma. */
export function openUrl(url: string) {
  const w = window.open(url, "_blank");
  if (w) w.opener = null;
  else location.href = url;
}
