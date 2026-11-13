//! Suscripciones con Dodo Payments: crear el pago y atender los avisos
//! (Standard Webhooks) que cuentan si sigue activa.

use crate::state::Shared;
use crate::util::now;
use crate::web::api;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

const WEBHOOK_TOLERANCE: i64 = 300;

pub(crate) async fn checkout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let Some(uid) = app.user_of(&headers) else { return crate::api::unauthorized() };
    let Some(u) = app.member(&uid).await else { return crate::api::not_member(&app) };
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

pub(crate) async fn webhook(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> StatusCode {
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
