//! Servicio de cuentas para el VPS: inicio de sesión con Discord, comprobación
//! de que el usuario está en el servidor, suscripciones con Dodo Payments y el
//! rol «Suscriptor» en Discord.
//!
//! - Público (detrás de Caddy con HTTPS): `/login`, `/callback`, `/paid`,
//!   `/api/*` y `/webhooks/dodo`.
//! - Interno (solo loopback, lo usa el repetidor): `/internal/redeem`.
//!
//! La configuración y los secretos llegan por variables de entorno (en el VPS,
//! `/etc/mestisexo/auth.env`). Lo que no esté configurado queda desactivado.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{middleware, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SESSION_TTL: u64 = 30 * 24 * 3600;
const LOGIN_TTL: u64 = 600;
const TICKET_TTL: u64 = 60;
/// Cada cuánto se vuelve a comprobar con el bot que el usuario sigue en el servidor.
const MEMBER_RECHECK: u64 = 3600;
/// Si Discord no responde, se acepta una comprobación de hasta este tiempo.
const MEMBER_GRACE: u64 = 24 * 3600;
const WEBHOOK_TOLERANCE: i64 = 300;
const MAX_PENDING: usize = 2000;
const MAX_NAME: usize = 24;
const DISCORD_API: &str = "https://discord.com/api/v10";

struct Discord {
    client_id: String,
    client_secret: String,
    bot_token: String,
    guild_id: String,
    role_id: Option<String>,
    invite: Option<String>,
}

struct Dodo {
    api_key: String,
    webhook_key: Vec<u8>,
    product_id: String,
    base: String,
}

struct Cfg {
    public_url: String,
    discord: Option<Discord>,
    dodo: Option<Dodo>,
    db_path: PathBuf,
}

#[derive(Serialize, Deserialize, Default)]
struct Db {
    users: HashMap<String, User>,
    /// Clave: SHA-256 del token de sesión (el token en sí no se guarda).
    sessions: HashMap<String, Session>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct User {
    name: String,
    sub: bool,
    sub_id: Option<String>,
    /// `webhook-timestamp` del último cambio aplicado, para ignorar avisos desordenados.
    sub_ts: i64,
    /// El rol de Discord coincide con `sub`.
    role_synced: bool,
    /// Última vez (s) que se comprobó que está en el servidor.
    checked: u64,
}

#[derive(Serialize, Deserialize)]
struct Session {
    user: String,
    exp: u64,
}

#[derive(Default)]
struct Mem {
    /// `state` de OAuth → (puerto local de la app o 0 si es la web, `state` de la app, caducidad).
    logins: HashMap<String, (u16, String, u64)>,
    /// Código de un solo uso → (token de sesión, caducidad).
    codes: HashMap<String, (String, u64)>,
    /// Ticket de un solo uso para entrar al stream → (usuario, caducidad).
    tickets: HashMap<String, (String, u64)>,
    /// `webhook-id` ya procesados.
    seen_hooks: HashMap<String, u64>,
    /// Último intento fallido de cambiar el rol, para no insistir a Discord en cada petición.
    role_retry: HashMap<String, u64>,
}

struct App {
    cfg: Cfg,
    http: reqwest::Client,
    db: Mutex<Db>,
    mem: Mutex<Mem>,
}

type Shared = Arc<App>;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn token(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

fn valid_token(s: &str) -> bool {
    (16..=64).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
}

fn hash(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect::<String>().trim().to_string()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl App {
    fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn mem(&self) -> std::sync::MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, db: &Db) {
        let tmp = self.cfg.db_path.with_extension("tmp");
        let res = serde_json::to_vec(db)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &self.cfg.db_path));
        if let Err(e) = res {
            eprintln!("No se pudo guardar {}: {e}", self.cfg.db_path.display());
        }
    }

    fn user_of(&self, headers: &HeaderMap) -> Option<String> {
        let t = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        if !valid_token(t) {
            return None;
        }
        let db = self.db();
        db.sessions.get(&hash(t)).filter(|s| s.exp > now()).map(|s| s.user.clone())
    }

    /// Nombre a mostrar si el usuario está en el servidor, `None` si no está.
    async fn fetch_member(&self, d: &Discord, uid: &str) -> Result<Option<String>, ()> {
        let r = self
            .http
            .get(format!("{DISCORD_API}/guilds/{}/members/{uid}", d.guild_id))
            .header(header::AUTHORIZATION, format!("Bot {}", d.bot_token))
            .send()
            .await
            .map_err(|_| ())?;
        match r.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => {
                let m: Value = r.json().await.map_err(|_| ())?;
                let name = [&m["nick"], &m["user"]["global_name"], &m["user"]["username"]]
                    .into_iter()
                    .find_map(|v| v.as_str().map(clean_name).filter(|n| !n.is_empty()))
                    .unwrap_or_else(|| "Usuario".into());
                Ok(Some(name))
            }
            s => {
                eprintln!("Discord respondió {s} al consultar un miembro");
                Err(())
            }
        }
    }

    /// Comprueba (como mucho una vez por hora) que sigue en el servidor y
    /// actualiza su nombre. Devuelve el usuario si puede entrar.
    async fn member(&self, uid: &str) -> Option<User> {
        let d = self.cfg.discord.as_ref()?;
        let checked = self.db().users.get(uid)?.checked;
        if now().saturating_sub(checked) >= MEMBER_RECHECK {
            match self.fetch_member(d, uid).await {
                Ok(Some(name)) => {
                    let mut db = self.db();
                    if let Some(u) = db.users.get_mut(uid) {
                        u.name = name;
                        u.checked = now();
                    }
                    self.save(&db);
                }
                Ok(None) => return None,
                Err(()) if now().saturating_sub(checked) < MEMBER_GRACE => {}
                Err(()) => return None,
            }
        }
        let u = self.db().users.get(uid).cloned()?;
        let retry_due = self.mem().role_retry.get(uid).map_or(true, |t| now() >= *t);
        if !u.role_synced && retry_due {
            self.sync_role(uid).await;
        }
        Some(u)
    }

    /// Pone o quita el rol de suscriptor según el estado guardado.
    async fn sync_role(&self, uid: &str) {
        let Some(d) = &self.cfg.discord else { return };
        let Some(role) = &d.role_id else { return };
        let Some(sub) = self.db().users.get(uid).map(|u| u.sub) else { return };
        let url = format!("{DISCORD_API}/guilds/{}/members/{uid}/roles/{role}", d.guild_id);
        let req = if sub { self.http.put(url) } else { self.http.delete(url) };
        let res = req
            .header(header::AUTHORIZATION, format!("Bot {}", d.bot_token))
            .header("X-Audit-Log-Reason", if sub { "Suscripción activa" } else { "Suscripción terminada" })
            .header(header::CONTENT_LENGTH, "0")
            .send()
            .await;
        // 404: ya no está en el servidor; no hay nada que sincronizar.
        let ok = matches!(&res, Ok(r) if r.status().is_success() || r.status() == StatusCode::NOT_FOUND);
        if ok {
            self.mem().role_retry.remove(uid);
        } else {
            eprintln!("No se pudo actualizar el rol de {uid}: {:?}", res.map(|r| r.status()));
            self.mem().role_retry.insert(uid.to_string(), now() + 300);
        }
        let mut db = self.db();
        if let Some(u) = db.users.get_mut(uid) {
            if u.sub == sub {
                u.role_synced = ok;
            }
        }
        self.save(&db);
    }
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"es\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{t} · Mestisexo</title><style>body{{background:#0e0e10;color:#efeff1;font:16px system-ui,sans-serif;\
         max-width:520px;margin:15vh auto;padding:0 20px}}a{{color:#a970ff}}</style></head>\
         <body><h1>{t}</h1>{body}</body></html>",
        t = esc(title)
    ))
}

fn api(status: StatusCode, v: Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], axum::Json(v)).into_response()
}

// ---------- Inicio de sesión con Discord ----------

/// La app escucha en `127.0.0.1:port` y recibe ahí el resultado (RFC 8252).
/// Así, aunque alguien envíe a otro un enlace de inicio de sesión preparado,
/// el código acaba en el PC de quien inicia sesión y no en el de quien lo envió.
/// Con `web=1` (versión web) el resultado vuelve a la página principal, que
/// solo lo acepta si el `state` coincide con el que guardó esa pestaña.
#[derive(Deserialize)]
struct LoginQuery {
    #[serde(default)]
    port: u16,
    /// `web=1`.
    web: Option<String>,
    state: String,
}

async fn login(State(app): State<Shared>, Query(q): Query<LoginQuery>) -> Response {
    let Some(d) = &app.cfg.discord else {
        return page("No disponible", "<p>El inicio de sesión con Discord aún no está configurado.</p>").into_response();
    };
    let web = q.web.as_deref() == Some("1");
    if !valid_token(&q.state) || (q.port < 1024 && !web) {
        return (StatusCode::BAD_REQUEST, page("Enlace inválido", "<p>Vuelve a la app e inténtalo de nuevo.</p>")).into_response();
    }
    let state = token(32);
    {
        let mut mem = app.mem();
        if mem.logins.len() >= MAX_PENDING {
            return (StatusCode::SERVICE_UNAVAILABLE, page("Ocupado", "<p>Inténtalo dentro de unos minutos.</p>")).into_response();
        }
        mem.logins.insert(state.clone(), (if web { 0 } else { q.port }, q.state, now() + LOGIN_TTL));
    }
    let url = reqwest::Url::parse_with_params(
        "https://discord.com/oauth2/authorize",
        &[
            ("client_id", d.client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", &format!("{}/callback", app.cfg.public_url)),
            ("scope", "identify"),
            ("state", &state),
        ],
    );
    match url {
        Ok(u) => Redirect::to(u.as_str()).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(State(app): State<Shared>, Query(q): Query<Callback>) -> Response {
    let Some(d) = &app.cfg.discord else { return StatusCode::NOT_FOUND.into_response() };
    let pending = q.state.as_deref().and_then(|s| app.mem().logins.remove(s)).filter(|(_, _, exp)| *exp > now());
    let Some((port, app_state, _)) = pending else {
        return page("Enlace caducado", "<p>Vuelve a la app y pulsa otra vez «Iniciar sesión con Discord».</p>").into_response();
    };
    let Some(code) = q.code else {
        return page("Inicio de sesión cancelado", "<p>Puedes cerrar esta pestaña.</p>").into_response();
    };

    let uid = match discord_user(&app, d, &code).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("OAuth de Discord: {e}");
            return page("Error", "<p>Discord no respondió como se esperaba. Vuelve a intentarlo.</p>").into_response();
        }
    };

    let name = match app.fetch_member(d, &uid).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            let link = d.invite.clone()
                .map(|i| format!("<p><a href=\"{0}\">{0}</a></p>", esc(&i)))
                .unwrap_or_default();
            return page("No estás en el servidor", &format!("<p>Para ver los directos tienes que unirte al servidor de Discord.</p>{link}<p>Después vuelve a la app e inicia sesión otra vez.</p>")).into_response();
        }
        Err(()) => {
            return page("Error", "<p>No se pudo comprobar tu cuenta en el servidor. Vuelve a intentarlo.</p>").into_response();
        }
    };

    let session = token(48);
    {
        let mut db = app.db();
        let u = db.users.entry(uid.clone()).or_insert_with(|| User { role_synced: true, ..Default::default() });
        u.name = name.clone();
        u.checked = now();
        db.sessions.insert(hash(&session), Session { user: uid.clone(), exp: now() + SESSION_TTL });
        app.save(&db);
    }
    let one_time = token(32);
    app.mem().codes.insert(one_time.clone(), (session, now() + TICKET_TTL * 5));
    if port == 0 {
        return Redirect::to(&format!("{}/?code={one_time}&state={app_state}", app.cfg.public_url)).into_response();
    }
    Redirect::to(&format!("http://127.0.0.1:{port}/done?code={one_time}&state={app_state}")).into_response()
}

async fn discord_user(app: &App, d: &Discord, code: &str) -> Result<String, String> {
    let redirect = format!("{}/callback", app.cfg.public_url);
    let tok: Value = app
        .http
        .post(format!("{DISCORD_API}/oauth2/token"))
        .basic_auth(&d.client_id, Some(&d.client_secret))
        .form(&[("grant_type", "authorization_code"), ("code", code), ("redirect_uri", &redirect)])
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let access = tok["access_token"].as_str().ok_or("sin access_token")?;
    let me: Value = app
        .http
        .get(format!("{DISCORD_API}/users/@me"))
        .bearer_auth(access)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let id = me["id"].as_str().ok_or("sin id")?;
    if id.is_empty() || id.len() > 24 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("id inválido".into());
    }
    Ok(id.to_string())
}

// ---------- API para la app ----------

#[derive(Deserialize)]
struct Exchange {
    code: String,
}

/// Cambia el código de un solo uso por el token de sesión.
async fn exchange(State(app): State<Shared>, Query(q): Query<Exchange>) -> Response {
    match app.mem().codes.remove(&q.code).filter(|(_, exp)| *exp > now()) {
        Some((token, _)) => api(StatusCode::OK, json!({ "token": token })),
        None => api(StatusCode::NOT_FOUND, json!({ "error": "code" })),
    }
}

fn unauthorized() -> Response {
    api(StatusCode::UNAUTHORIZED, json!({ "error": "session" }))
}

fn not_member(app: &App) -> Response {
    let invite = app.cfg.discord.as_ref().and_then(|d| d.invite.clone());
    api(StatusCode::FORBIDDEN, json!({ "error": "not_member", "invite": invite }))
}

async fn me(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let Some(uid) = app.user_of(&headers) else { return unauthorized() };
    let Some(u) = app.member(&uid).await else { return not_member(&app) };
    api(StatusCode::OK, json!({ "id": uid, "name": u.name, "sub": u.sub, "payments": app.cfg.dodo.is_some() }))
}

async fn ticket(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let Some(uid) = app.user_of(&headers) else { return unauthorized() };
    if app.member(&uid).await.is_none() {
        return not_member(&app);
    }
    let t = token(32);
    {
        let mut mem = app.mem();
        if mem.tickets.len() >= MAX_PENDING {
            return api(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "busy" }));
        }
        mem.tickets.insert(t.clone(), (uid, now() + TICKET_TTL));
    }
    api(StatusCode::OK, json!({ "ticket": t }))
}

async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        let mut db = app.db();
        if db.sessions.remove(&hash(t)).is_some() {
            app.save(&db);
        }
    }
    api(StatusCode::OK, json!({ "ok": true }))
}

async fn checkout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let Some(uid) = app.user_of(&headers) else { return unauthorized() };
    let Some(u) = app.member(&uid).await else { return not_member(&app) };
    let Some(dodo) = &app.cfg.dodo else {
        return api(StatusCode::SERVICE_UNAVAILABLE, json!({ "error": "Los pagos aún no están configurados." }));
    };
    if u.sub {
        return api(StatusCode::CONFLICT, json!({ "error": "Ya tienes una suscripción activa." }));
    }
    let body = json!({
        "product_cart": [{ "product_id": dodo.product_id, "quantity": 1 }],
        "metadata": { "discord_id": uid },
        "return_url": format!("{}/paid", app.cfg.public_url),
    });
    let res = app
        .http
        .post(format!("{}/checkouts", dodo.base))
        .bearer_auth(&dodo.api_key)
        .json(&body)
        .send()
        .await;
    let v: Option<Value> = match res {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        Ok(r) => {
            let s = r.status();
            eprintln!("Dodo respondió {s} al crear el pago: {}", r.text().await.unwrap_or_default());
            None
        }
        Err(e) => {
            eprintln!("Dodo: {e}");
            None
        }
    };
    match v.as_ref().and_then(|v| v["checkout_url"].as_str()) {
        Some(url) if url.starts_with("https://") => api(StatusCode::OK, json!({ "url": url })),
        _ => api(StatusCode::BAD_GATEWAY, json!({ "error": "No se pudo crear el pago. Inténtalo más tarde." })),
    }
}

async fn paid() -> Html<String> {
    page("Gracias", "<p>Si el pago se completó, tu insignia de suscriptor aparecerá en la app en unos segundos. Ya puedes cerrar esta pestaña.</p><p><a href=\"/\">Volver al directo</a></p>")
}

/// La app hace las peticiones desde su webview (otro origen). No se usan
/// cookies, solo el token en `Authorization`, así que se permite cualquier origen.
async fn cors(req: axum::extract::Request, next: middleware::Next) -> Response {
    let mut res = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    res
}

// ---------- Avisos de Dodo Payments (Standard Webhooks) ----------

fn verify_webhook(key: &[u8], headers: &HeaderMap, body: &[u8]) -> Result<(String, i64), &'static str> {
    let get = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).ok_or("faltan cabeceras");
    let (id, ts, sigs) = (get("webhook-id")?, get("webhook-timestamp")?, get("webhook-signature")?);
    let t: i64 = ts.parse().map_err(|_| "marca de tiempo inválida")?;
    if (now() as i64 - t).abs() > WEBHOOK_TOLERANCE {
        return Err("marca de tiempo fuera de plazo");
    }
    for s in sigs.split(' ') {
        let Some(sig) = s.strip_prefix("v1,").and_then(|b| B64.decode(b).ok()) else { continue };
        let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| "clave inválida")?;
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(ts.as_bytes());
        mac.update(b".");
        mac.update(body);
        if mac.verify_slice(&sig).is_ok() {
            return Ok((id.to_string(), t));
        }
    }
    Err("firma inválida")
}

/// Estados con los que se mantiene la suscripción (en `past_due` Dodo da un
/// plazo de gracia antes de cortarla).
fn sub_active(status: &str) -> bool {
    matches!(status, "active" | "past_due")
}

async fn webhook(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let Some(dodo) = &app.cfg.dodo else { return StatusCode::NOT_FOUND };
    let (id, ts) = match verify_webhook(&dodo.webhook_key, &headers, &body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Aviso de Dodo rechazado: {e}");
            return StatusCode::UNAUTHORIZED;
        }
    };
    {
        let mut mem = app.mem();
        if mem.seen_hooks.contains_key(&id) {
            return StatusCode::OK;
        }
        mem.seen_hooks.insert(id, now() + 2 * WEBHOOK_TOLERANCE as u64);
    }
    let Ok(v) = serde_json::from_slice::<Value>(&body) else { return StatusCode::BAD_REQUEST };
    let kind = v["type"].as_str().unwrap_or("");
    if !kind.starts_with("subscription.") {
        return StatusCode::OK;
    }
    let data = &v["data"];
    let (Some(sub_id), Some(status)) = (data["subscription_id"].as_str(), data["status"].as_str()) else {
        return StatusCode::OK;
    };
    let active = sub_active(status);

    let changed_user = {
        let mut db = app.db();
        let uid = data["metadata"]["discord_id"]
            .as_str()
            .filter(|u| db.users.contains_key(*u))
            .map(String::from)
            .or_else(|| db.users.iter().find(|(_, u)| u.sub_id.as_deref() == Some(sub_id)).map(|(k, _)| k.clone()));
        let Some(uid) = uid else {
            eprintln!("Aviso {kind} de la suscripción {sub_id} sin usuario conocido");
            return StatusCode::OK;
        };
        let u = db.users.get_mut(&uid).expect("existe");
        // Un aviso de otra suscripción anterior del mismo usuario no debe quitarle la actual.
        let other_sub = u.sub && u.sub_id.as_deref().is_some_and(|s| s != sub_id);
        if ts < u.sub_ts || (other_sub && !active) {
            return StatusCode::OK;
        }
        u.sub_ts = ts;
        u.sub_id = Some(sub_id.to_string());
        let changed = u.sub != active;
        u.sub = active;
        if changed {
            u.role_synced = false;
        }
        eprintln!("Suscripción {sub_id} de {uid}: {status}");
        app.save(&db);
        changed.then_some(uid)
    };
    if let Some(uid) = changed_user {
        app.sync_role(&uid).await;
    }
    StatusCode::OK
}

// ---------- Interno: el repetidor canjea el ticket del espectador ----------

#[derive(Deserialize)]
struct Redeem {
    t: String,
}

async fn redeem(State(app): State<Shared>, Query(q): Query<Redeem>) -> Response {
    let entry = app.mem().tickets.remove(&q.t).filter(|(_, exp)| *exp > now());
    let Some((uid, _)) = entry else { return StatusCode::NOT_FOUND.into_response() };
    match app.db().users.get(&uid) {
        Some(u) => api(StatusCode::OK, json!({ "id": uid, "name": u.name, "sub": u.sub })),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn prune(app: &App) {
    let t = now();
    {
        let mut mem = app.mem();
        mem.logins.retain(|_, (_, _, e)| *e > t);
        mem.codes.retain(|_, (_, e)| *e > t);
        mem.tickets.retain(|_, (_, e)| *e > t);
        mem.seen_hooks.retain(|_, e| *e > t);
        mem.role_retry.retain(|_, e| *e > t);
    }
    let mut db = app.db();
    let before = db.sessions.len();
    db.sessions.retain(|_, s| s.exp > t);
    if db.sessions.len() != before {
        app.save(&db);
    }
}

fn load_cfg() -> Result<Cfg, String> {
    let public_url = env("PUBLIC_URL").ok_or("falta PUBLIC_URL")?.trim_end_matches('/').to_string();
    if !public_url.starts_with("https://") {
        return Err("PUBLIC_URL debe empezar por https://".into());
    }
    let discord = match (env("DISCORD_CLIENT_ID"), env("DISCORD_CLIENT_SECRET"), env("DISCORD_BOT_TOKEN"), env("DISCORD_GUILD_ID")) {
        (Some(client_id), Some(client_secret), Some(bot_token), Some(guild_id)) => Some(Discord {
            client_id,
            client_secret,
            bot_token,
            guild_id,
            role_id: env("DISCORD_SUB_ROLE_ID"),
            invite: env("DISCORD_INVITE").filter(|i| i.starts_with("https://")),
        }),
        _ => None,
    };
    let dodo = match (env("DODO_API_KEY"), env("DODO_WEBHOOK_SECRET"), env("DODO_PRODUCT_ID")) {
        (Some(api_key), Some(secret), Some(product_id)) => {
            let webhook_key = B64
                .decode(secret.strip_prefix("whsec_").unwrap_or(&secret))
                .map_err(|_| "DODO_WEBHOOK_SECRET no es válido")?;
            let base = match env("DODO_MODE").as_deref() {
                Some("live") => "https://live.dodopayments.com",
                _ => "https://test.dodopayments.com",
            };
            Some(Dodo { api_key, webhook_key, product_id, base: base.into() })
        }
        _ => None,
    };
    let dir = env("STATE_DIRECTORY").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    Ok(Cfg { public_url, discord, dodo, db_path: dir.join("auth.json") })
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cfg = load_cfg().unwrap_or_else(|e| {
        eprintln!("Configuración: {e}");
        std::process::exit(2);
    });
    let listen = env("LISTEN").unwrap_or_else(|| "127.0.0.1:8092".into());
    let internal = env("INTERNAL_LISTEN").unwrap_or_else(|| "127.0.0.1:8093".into());
    eprintln!(
        "Discord: {}. Pagos: {}.",
        if cfg.discord.is_some() { "configurado" } else { "sin configurar" },
        match &cfg.dodo { Some(d) => d.base.as_str(), None => "sin configurar" }
    );

    let db: Db = std::fs::read(&cfg.db_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("DiscordBot (https://github.com/eymestii/mestisexo, 0.2)")
        .build()
        .expect("cliente HTTP");
    let app = Arc::new(App { cfg, http, db: Mutex::new(db), mem: Mutex::default() });

    let api_routes = Router::new()
        .route("/api/exchange", axum::routing::post(exchange))
        .route("/api/me", get(me))
        .route("/api/ticket", axum::routing::post(ticket))
        .route("/api/checkout", axum::routing::post(checkout))
        .route("/api/logout", axum::routing::post(logout))
        .layer(middleware::from_fn(cors));
    let public = Router::new()
        .route("/login", get(login))
        .route("/callback", get(callback))
        .route("/paid", get(paid))
        .route("/webhooks/dodo", axum::routing::post(webhook))
        .merge(api_routes)
        .with_state(app.clone());
    let private = Router::new().route("/internal/redeem", get(redeem)).with_state(app.clone());

    let pub_l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
        eprintln!("No se pudo abrir {listen}: {e}");
        std::process::exit(1);
    });
    let int_l = tokio::net::TcpListener::bind(&internal).await.unwrap_or_else(|e| {
        eprintln!("No se pudo abrir {internal}: {e}");
        std::process::exit(1);
    });
    eprintln!("Servicio de cuentas en {listen} (interno {internal})");

    let pruner = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            prune(&pruner);
        }
    });
    let (a, b) = tokio::join!(axum::serve(pub_l, public), axum::serve(int_l, private));
    if let Err(e) = a.and(b) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
