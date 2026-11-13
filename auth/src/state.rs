//! Estado del servicio: lo que se guarda en disco (`auth.json`) y lo que vive
//! solo en memoria mientras corre el proceso.

use crate::config::Cfg;
use crate::util::{hash, now, valid_token};
use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) const SESSION_TTL: u64 = 30 * 24 * 3600;
pub(crate) const TICKET_TTL: u64 = 60;
pub(crate) const MAX_PENDING: usize = 2000;

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Db {
    pub(crate) users: HashMap<String, User>,
    /// Clave: SHA-256 del token de sesión (el token en sí no se guarda).
    pub(crate) sessions: HashMap<String, Session>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub(crate) struct User {
    pub(crate) name: String,
    pub(crate) sub: bool,
    pub(crate) sub_id: Option<String>,
    /// `webhook-timestamp` del último cambio aplicado, para ignorar avisos desordenados.
    pub(crate) sub_ts: i64,
    /// El rol de Discord coincide con `sub`.
    pub(crate) role_synced: bool,
    /// Última vez (s) que se comprobó que está en el servidor.
    pub(crate) checked: u64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Session {
    pub(crate) user: String,
    pub(crate) exp: u64,
}

#[derive(Default)]
pub(crate) struct Mem {
    /// `state` de OAuth → (`state` de la página, caducidad).
    pub(crate) logins: HashMap<String, (String, u64)>,
    /// Código de un solo uso → (token de sesión, caducidad).
    pub(crate) codes: HashMap<String, (String, u64)>,
    /// Ticket de un solo uso para entrar al stream → (usuario, caducidad).
    pub(crate) tickets: HashMap<String, (String, u64)>,
    /// `webhook-id` ya procesados.
    pub(crate) seen_hooks: HashMap<String, u64>,
    /// Último intento fallido de cambiar el rol, para no insistir a Discord en cada petición.
    pub(crate) role_retry: HashMap<String, u64>,
}

pub(crate) struct App {
    pub(crate) cfg: Cfg,
    pub(crate) http: reqwest::Client,
    pub(crate) db: Mutex<Db>,
    pub(crate) mem: Mutex<Mem>,
}

pub(crate) type Shared = Arc<App>;

impl App {
    pub(crate) fn db(&self) -> std::sync::MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn mem(&self) -> std::sync::MutexGuard<'_, Mem> {
        self.mem.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn save(&self, db: &Db) {
        let tmp = self.cfg.db_path.with_extension("tmp");
        let res = serde_json::to_vec(db)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &self.cfg.db_path));
        if let Err(e) = res {
            eprintln!("No se pudo guardar {}: {e}", self.cfg.db_path.display());
        }
    }

    pub(crate) fn user_of(&self, headers: &HeaderMap) -> Option<String> {
        let t = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?;
        if !valid_token(t) {
            return None;
        }
        let db = self.db();
        db.sessions.get(&hash(t)).filter(|s| s.exp > now()).map(|s| s.user.clone())
    }
}

/// Tira lo caducado. Se llama cada minuto desde `main`.
pub(crate) fn prune(app: &App) {
    let t = now();
    {
        let mut mem = app.mem();
        mem.logins.retain(|_, (_, e)| *e > t);
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
