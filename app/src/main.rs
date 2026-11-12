#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mestisexo_core as core;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

#[derive(Default)]
struct App {
    session: Mutex<Option<core::Handle>>,
    relay: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Serialize)]
struct Session {
    rtmp_server: String,
    stream_key: String,
    ws_port: u16,
    key: String,
    host_token: String,
    vps_host: Option<String>,
    tailscale_ip: Option<String>,
    lan_ip: Option<String>,
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
    let vps = vps.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let vps_host = match &vps {
        Some(v) => {
            // Solo usuario@host o host: nada que ssh pueda interpretar como opción.
            let ok = !v.starts_with('-')
                && v.len() <= 255
                && v.chars().all(|c| c.is_ascii_alphanumeric() || "@.-_:[]".contains(c));
            if !ok {
                return Err("Destino del VPS inválido, usa usuario@ip".into());
            }
            Some(v.rsplit('@').next().unwrap_or(v).to_string())
        }
        None => None,
    };
    let tunnel = vps.map(|target| core::Tunnel {
        target,
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
        vps_host,
        tailscale_ip: core::tailscale_ip().map(|ip| ip.to_string()),
        lan_ip: core::local_ip().map(|ip| ip.to_string()),
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

#[tauri::command]
async fn clear_chat(app: State<'_, App>) -> Result<(), String> {
    if let Some(h) = app.session.lock().await.as_ref() {
        h.clear_chat();
    }
    Ok(())
}

#[tauri::command]
async fn connect_viewer(app: State<'_, App>, addr: String) -> Result<u16, String> {
    let addr = addr.trim().to_string();
    if addr.is_empty() || addr.len() > 255 || !addr.contains(':') {
        return Err("Dirección inválida, usa ip:puerto".into());
    }
    let mut slot = app.relay.lock().await;
    if let Some(t) = slot.take() {
        t.abort();
    }
    let (port, task) = core::relay(addr).await.map_err(|e| e.to_string())?;
    *slot = Some(task);
    Ok(port)
}

#[tauri::command]
async fn disconnect_viewer(app: State<'_, App>) -> Result<(), String> {
    if let Some(t) = app.relay.lock().await.take() {
        t.abort();
    }
    Ok(())
}

/// Abre una dirección https:// en el navegador del sistema.
fn open_browser(url: &str) -> Result<(), String> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c == '"') {
        return Err("Dirección no válida".into());
    }
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]).creation_flags(0x0800_0000);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| format!("No se pudo abrir el navegador: {e}"))
}

#[tauri::command]
async fn open_url(url: String) -> Result<(), String> {
    open_browser(&url)
}

/// Inicio de sesión con Discord: abre el navegador en el servicio de cuentas
/// y espera a que este redirija a un puerto local con un código de un solo uso.
#[tauri::command]
async fn discord_login(auth_url: String) -> Result<String, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let base = auth_url.trim_end_matches('/');
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let state = core::random_token(32);
    open_browser(&format!("{base}/login?port={port}&state={state}"))?;

    let wait = async {
        loop {
            let (mut s, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 4096];
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut buf)).await.unwrap_or(Ok(0)).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("");
            let query = path.strip_prefix("/done?").unwrap_or("");
            let param = |k: &str| query.split('&').find_map(|kv| kv.split_once('=').filter(|(a, _)| *a == k).map(|(_, v)| v.to_string()));
            let ok = param("state").as_deref() == Some(state.as_str());
            let code = param("code").filter(|c| ok && !c.is_empty() && c.len() <= 64 && c.chars().all(|c| c.is_ascii_alphanumeric()));
            let body = if code.is_some() {
                "<!doctype html><meta charset=utf-8><title>Mestisexo</title><body style=\"background:#0e0e10;color:#efeff1;font:16px system-ui;text-align:center;margin-top:20vh\"><h1>Sesión iniciada</h1><p>Ya puedes cerrar esta pestaña y volver a Mestisexo.</p>"
            } else {
                "<!doctype html><meta charset=utf-8><title>Mestisexo</title><p>Enlace no válido.</p>"
            };
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = s.write_all(resp.as_bytes()).await;
            if let Some(c) = code {
                return Ok::<String, String>(c);
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(300), wait)
        .await
        .map_err(|_| "Se agotó el tiempo para iniciar sesión".to_string())?
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
            stream_status,
            clear_chat,
            connect_viewer,
            disconnect_viewer,
            open_url,
            discord_login
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
