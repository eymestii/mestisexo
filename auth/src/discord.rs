//! Todo lo que habla con Discord: iniciar sesión por OAuth2, comprobar que el
//! usuario está en el servidor y mantener el rol de suscriptor.

use crate::config::Discord;
use crate::state::{App, Session, Shared, User, MAX_PENDING, SESSION_TTL, TICKET_TTL};
use crate::util::{hash, now, token, valid_token};
use crate::web::{esc, page};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::Value;

const DISCORD_API: &str = "https://discord.com/api/v10";
const LOGIN_TTL: u64 = 600;
/// Cada cuánto se vuelve a comprobar con el bot que el usuario sigue en el servidor.
const MEMBER_RECHECK: u64 = 3600;
/// Si Discord no responde, se acepta una comprobación de hasta este tiempo.
const MEMBER_GRACE: u64 = 24 * 3600;
const MAX_NAME: usize = 24;

fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect::<String>().trim().to_string()
}

impl App {
    /// Nombre a mostrar si el usuario está en el servidor, `None` si no está.
    pub(crate) async fn fetch_member(&self, d: &Discord, uid: &str) -> Result<Option<String>, ()> {
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
    pub(crate) async fn member(&self, uid: &str) -> Option<User> {
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
    pub(crate) async fn sync_role(&self, uid: &str) {
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

/// El resultado vuelve a la página principal, que solo lo acepta si el
/// `state` coincide con el que guardó ese navegador antes de salir. Así, un
/// enlace de inicio de sesión preparado por otro no sirve.
#[derive(Deserialize)]
pub(crate) struct LoginQuery {
    state: String,
}

pub(crate) async fn login(State(app): State<Shared>, Query(q): Query<LoginQuery>) -> Response {
    let Some(d) = &app.cfg.discord else {
        return page("No disponible", "<p>El inicio de sesión con Discord aún no está configurado.</p>").into_response();
    };
    if !valid_token(&q.state) {
        return (StatusCode::BAD_REQUEST, page("Enlace inválido", "<p><a href=\"/\">Vuelve al directo</a> e inténtalo de nuevo.</p>")).into_response();
    }
    let state = token(32);
    {
        let mut mem = app.mem();
        if mem.logins.len() >= MAX_PENDING {
            return (StatusCode::SERVICE_UNAVAILABLE, page("Ocupado", "<p>Inténtalo dentro de unos minutos.</p>")).into_response();
        }
        mem.logins.insert(state.clone(), (q.state, now() + LOGIN_TTL));
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
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
}

pub(crate) async fn callback(State(app): State<Shared>, Query(q): Query<Callback>) -> Response {
    let Some(d) = &app.cfg.discord else { return StatusCode::NOT_FOUND.into_response() };
    let pending = q.state.as_deref().and_then(|s| app.mem().logins.remove(s)).filter(|(_, exp)| *exp > now());
    let Some((app_state, _)) = pending else {
        return page("Enlace caducado", "<p><a href=\"/\">Vuelve al directo</a> y pulsa otra vez «Iniciar sesión con Discord».</p>").into_response();
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
            return page("No estás en el servidor", &format!("<p>Para ver los directos tienes que unirte al servidor de Discord.</p>{link}<p>Después <a href=\"/\">vuelve al directo</a> e inicia sesión otra vez.</p>")).into_response();
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
    Redirect::to(&format!("{}/?code={one_time}&state={app_state}", app.cfg.public_url)).into_response()
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
