#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mestisexo_core as core;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;

#[derive(Default)]
struct App {
    session: Mutex<Option<core::Handle>>,
}

#[derive(Serialize)]
struct Session {
    rtmp_server: String,
    stream_key: String,
    ws_port: u16,
    key: String,
    host_token: String,
}

/// Usa un ffmpeg incluido junto al ejecutable si existe; si no, el del PATH.
fn ffmpeg_path() -> String {
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string())
}

fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"))) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Claves fijas guardadas entre sesiones, en el directorio de configuración de la app.
#[derive(Serialize, Deserialize, Clone)]
struct Keys {
    viewer_key: String,
    stream_key: String,
}

fn keys_path(handle: &AppHandle) -> Result<PathBuf, String> {
    let dir = handle.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("keys.json"))
}

fn valid_key(k: &str, min: usize) -> bool {
    k.len() >= min && k.len() <= 64 && k.chars().all(|c| c.is_ascii_alphanumeric())
}

fn save_keys(handle: &AppHandle, keys: &Keys) -> Result<(), String> {
    let path = keys_path(handle)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(keys).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

fn load_keys(handle: &AppHandle) -> Result<Keys, String> {
    let path = keys_path(handle)?;
    if let Some(k) = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Keys>(&b).ok())
        .filter(|k| valid_key(&k.viewer_key, 8) && valid_key(&k.stream_key, 12))
    {
        return Ok(k);
    }
    let k = Keys { viewer_key: core::random_token(10), stream_key: core::random_token(16) };
    save_keys(handle, &k)?;
    Ok(k)
}

#[tauri::command]
async fn get_keys(handle: AppHandle) -> Result<Keys, String> {
    load_keys(&handle)
}

/// Genera claves nuevas. `which`: "viewer", "stream" o "both".
#[tauri::command]
async fn regenerate_keys(handle: AppHandle, which: String) -> Result<Keys, String> {
    let mut k = load_keys(&handle)?;
    if which == "viewer" || which == "both" {
        k.viewer_key = core::random_token(10);
    }
    if which == "stream" || which == "both" {
        k.stream_key = core::random_token(16);
    }
    save_keys(&handle, &k)?;
    Ok(k)
}

#[tauri::command]
async fn start_stream(
    handle: AppHandle,
    app: State<'_, App>,
    ws_port: u16,
    rtmp_port: u16,
    vps: Option<String>,
    vps_key: Option<String>,
) -> Result<Session, String> {
    let Some(vps) = vps.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
        return Err("Indica el VPS (usuario@ip): los espectadores entran por su web".into());
    };
    // Solo usuario@host o host: nada que ssh pueda interpretar como opción.
    let ok = !vps.starts_with('-')
        && vps.len() <= 255
        && vps.chars().all(|c| c.is_ascii_alphanumeric() || "@.-_:[]".contains(c));
    if !ok {
        return Err("Destino del VPS inválido, usa usuario@ip".into());
    }
    let tunnel = Some(core::Tunnel {
        target: vps,
        identity: vps_key.map(|k| expand_home(k.trim())).filter(|k| !k.is_empty()),
        upstream_port: core::REPEATER_UPSTREAM_PORT,
    });
    let keys = load_keys(&handle)?;
    let mut slot = app.session.lock().await;
    if let Some(old) = slot.take() {
        old.stop().await;
    }
    let h = core::start(core::Config {
        ws_port,
        rtmp_port,
        ffmpeg: ffmpeg_path(),
        tunnel,
        viewer_key: Some(keys.viewer_key),
        stream_key: Some(keys.stream_key),
    })
        .await
        .map_err(|e| e.to_string())?;
    let s = Session {
        rtmp_server: h.rtmp_server.clone(),
        stream_key: h.stream_key.clone(),
        ws_port: h.ws_port,
        key: h.key(),
        host_token: h.host_token().to_string(),
    };
    *slot = Some(h);
    Ok(s)
}

#[tauri::command]
async fn stop_stream(app: State<'_, App>) -> Result<(), String> {
    if let Some(h) = app.session.lock().await.take() {
        h.stop().await;
    }
    Ok(())
}

#[tauri::command]
async fn stream_status(app: State<'_, App>) -> Result<Option<core::Status>, String> {
    Ok(app.session.lock().await.as_ref().map(|h| h.status()))
}

fn main() {
    // WebKitGTK con NVIDIA suele dejar la ventana en blanco con el renderizador DMA-BUF.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    tauri::Builder::default()
        .manage(App::default())
        .invoke_handler(tauri::generate_handler![
            start_stream,
            get_keys,
            regenerate_keys,
            stop_stream,
            stream_status
        ])
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::Destroyed = e {
                let app = w.app_handle().state::<App>();
                let session = app.session.try_lock().ok().and_then(|mut s| s.take());
                drop(session);
            }
        })
        .run(tauri::generate_context!())
        .expect("error al iniciar la aplicación");
}
