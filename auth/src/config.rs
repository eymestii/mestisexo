//! Configuración y secretos. Todo llega por variables de entorno (en el VPS,
//! `/etc/mestisexo/auth.env` y `/etc/mestisexo/hls.env`) y lo que no esté
//! configurado queda desactivado.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use std::path::PathBuf;

pub(crate) struct Discord {
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
    pub(crate) bot_token: String,
    pub(crate) guild_id: String,
    pub(crate) role_id: Option<String>,
    pub(crate) invite: Option<String>,
}

pub(crate) struct Dodo {
    pub(crate) api_key: String,
    pub(crate) webhook_key: Vec<u8>,
    pub(crate) product_id: String,
    pub(crate) base: String,
}

pub(crate) struct Cfg {
    pub(crate) public_url: String,
    pub(crate) discord: Option<Discord>,
    pub(crate) dodo: Option<Dodo>,
    pub(crate) db_path: PathBuf,
    /// Clave compartida con el repetidor para firmar el acceso al vídeo.
    pub(crate) hls_secret: Option<Vec<u8>>,
}

pub(crate) fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub(crate) fn load_cfg() -> Result<Cfg, String> {
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
    // La misma clave que recibe el repetidor en HLS_SECRET. Si falta, el vídeo
    // no se firma y queda accesible a quien tenga la URL.
    let hls_secret = match env("HLS_SECRET") {
        Some(s) if s.len() < 32 => return Err("HLS_SECRET debe tener al menos 32 caracteres".into()),
        s => s.map(String::into_bytes),
    };
    Ok(Cfg { public_url, discord, dodo, db_path: dir.join("auth.json"), hls_secret })
}
