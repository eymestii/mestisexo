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
//!
//! Los dos servidores son dos routers distintos sobre el mismo estado: lo que
//! está en `private` no es alcanzable desde internet ni por error de Caddy.

mod api;
mod config;
mod discord;
mod payments;
mod state;
mod util;
mod web;

use axum::routing::get;
use axum::{middleware, Router};
use state::{App, Db};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cfg = config::load_cfg().unwrap_or_else(|e| {
        eprintln!("Configuración: {e}");
        std::process::exit(2);
    });
    let listen = config::env("LISTEN").unwrap_or_else(|| "127.0.0.1:8092".into());
    let internal = config::env("INTERNAL_LISTEN").unwrap_or_else(|| "127.0.0.1:8093".into());
    eprintln!(
        "Discord: {}. Pagos: {}. Vídeo firmado: {}.",
        if cfg.discord.is_some() { "configurado" } else { "sin configurar" },
        match &cfg.dodo { Some(d) => d.base.as_str(), None => "sin configurar" },
        if cfg.hls_secret.is_some() { "sí" } else { "no (HLS_SECRET sin definir)" }
    );

    let db: Db = std::fs::read(&cfg.db_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("DiscordBot (https://github.com/eymestii/mestisexo, 0.2)")
        .build()
        .expect("cliente HTTP");
    let app = Arc::new(App { cfg, http, db: Mutex::new(db), mem: Mutex::default() });

    let api_routes = Router::new()
        .route("/api/exchange", axum::routing::post(api::exchange))
        .route("/api/me", get(api::me))
        .route("/api/ticket", axum::routing::post(api::ticket))
        .route("/api/checkout", axum::routing::post(payments::checkout))
        .route("/api/logout", axum::routing::post(api::logout))
        .layer(middleware::from_fn(web::cors));
    let public = Router::new()
        .route("/login", get(discord::login))
        .route("/callback", get(discord::callback))
        .route("/paid", get(web::paid))
        .route("/webhooks/dodo", axum::routing::post(payments::webhook))
        .merge(api_routes)
        .with_state(app.clone());
    let private = Router::new().route("/internal/redeem", get(api::redeem)).with_state(app.clone());

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
            state::prune(&pruner);
        }
    });
    let (a, b) = tokio::join!(axum::serve(pub_l, public), axum::serve(int_l, private));
    if let Err(e) = a.and(b) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
