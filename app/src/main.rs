#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use mestisexo_core as core;
use serde::Serialize;
use tauri::{Manager, State};
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

#[tauri::command]
async fn start_stream(
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
        remote_port: ws_port,
    });
    let mut slot = app.session.lock().await;
    if let Some(old) = slot.take() {
        old.stop().await;
    }
    let h = core::start(core::Config { ws_port, rtmp_port, ffmpeg: ffmpeg_path(), tunnel })
        .await
        .map_err(|e| e.to_string())?;
    let s = Session {
        rtmp_server: h.rtmp_server.clone(),
        stream_key: h.stream_key.clone(),
        ws_port: h.ws_port,
        key: h.key().to_string(),
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
            stop_stream,
            stream_status,
            clear_chat,
            connect_viewer,
            disconnect_viewer
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
