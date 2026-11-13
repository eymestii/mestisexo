//! Lo que llama la web (`/api/*`) y lo que llama el repetidor por loopback
//! (`/internal/redeem`).

use crate::state::{App, Shared, MAX_PENDING, TICKET_TTL};
use crate::util::{hash, now, token};
use crate::web::api;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

/// Validez del token que abre los ficheros de vídeo. Corto para que un enlace
/// copiado no sirva mucho, y largo para que el reproductor no lo renueve a cada
/// rato: la web lo pide de nuevo antes de que caduque.
const VIDEO_TTL: u64 = 600;

pub(crate) fn unauthorized() -> Response {
    api(StatusCode::UNAUTHORIZED, json!({ "error": "session" }))
}

pub(crate) fn not_member(app: &App) -> Response {
    let invite = app.cfg.discord.as_ref().and_then(|d| d.invite.clone());
    api(StatusCode::FORBIDDEN, json!({ "error": "not_member", "invite": invite }))
}

#[derive(Deserialize)]
pub(crate) struct Exchange {
    code: String,
}

/// Cambia el código de un solo uso por el token de sesión.
pub(crate) async fn exchange(State(app): State<Shared>, Query(q): Query<Exchange>) -> Response {
    match app.mem().codes.remove(&q.code).filter(|(_, exp)| *exp > now()) {
        Some((token, _)) => api(StatusCode::OK, json!({ "token": token })),
        None => api(StatusCode::NOT_FOUND, json!({ "error": "code" })),
    }
}

pub(crate) async fn me(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let Some(uid) = app.user_of(&headers) else { return unauthorized() };
    let Some(u) = app.member(&uid).await else { return not_member(&app) };
    api(StatusCode::OK, json!({ "id": uid, "name": u.name, "sub": u.sub, "payments": app.cfg.dodo.is_some() }))
}

/// Permiso para entrar: el ticket de un solo uso abre el chat (lo canjea el
/// repetidor) y el token firmado abre los ficheros de vídeo. Son distintos
/// porque el vídeo son muchas peticiones y un ticket de un solo uso no vale.
pub(crate) async fn ticket(State(app): State<Shared>, headers: HeaderMap) -> Response {
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
    let exp = now() + VIDEO_TTL;
    let video = app.cfg.hls_secret.as_deref().and_then(|s| mestisexo_core::sign_token(s, exp));
    api(StatusCode::OK, json!({ "ticket": t, "video": video, "video_exp": exp }))
}

pub(crate) async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(t) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        let mut db = app.db();
        if db.sessions.remove(&hash(t)).is_some() {
            app.save(&db);
        }
    }
    api(StatusCode::OK, json!({ "ok": true }))
}

#[derive(Deserialize)]
pub(crate) struct Redeem {
    t: String,
}

pub(crate) async fn redeem(State(app): State<Shared>, Query(q): Query<Redeem>) -> Response {
    let entry = app.mem().tickets.remove(&q.t).filter(|(_, exp)| *exp > now());
    let Some((uid, _)) = entry else { return StatusCode::NOT_FOUND.into_response() };
    match app.db().users.get(&uid) {
        Some(u) => api(StatusCode::OK, json!({ "id": uid, "name": u.name, "sub": u.sub })),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
