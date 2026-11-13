//! Cadena completa del repetidor: entra la señal del streamer por el protocolo
//! de tramas y sale vídeo por HTTP, con el aviso del directo por el chat.
//!
//! `cargo test -p mestisexo-core --test repeater_hls` (necesita ffmpeg en el PATH).

use futures_util::StreamExt;
use mestisexo_core::testing as t;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

fn sample_mp4(seconds: u32) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", &format!("testsrc=size=320x240:rate=30:duration={seconds}")])
        .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}")])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "60"])
        .args(["-c:a", "aac", "-b:a", "64k"])
        .args([
            "-f", "mp4", "-frag_duration", "250000",
            "-movflags", "frag_keyframe+empty_moov+default_base_moof",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .expect("ffmpeg no está disponible");
    assert!(out.status.success(), "ffmpeg falló");
    out.stdout
}

/// Tres puertos libres. Se piden al sistema y se sueltan: es lo que hay para
/// no fijar puertos a mano en una prueba.
async fn free_ports() -> (SocketAddr, SocketAddr, SocketAddr) {
    let mut v = Vec::new();
    for _ in 0..3 {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("puerto libre");
        v.push((l.local_addr().expect("dirección"), l));
    }
    let addrs: Vec<SocketAddr> = v.iter().map(|(a, _)| *a).collect();
    drop(v);
    (addrs[0], addrs[1], addrs[2])
}

/// `[tipo u8][longitud u32][datos]`, el protocolo del enlace PC → VPS.
async fn frame(w: &mut TcpStream, kind: u8, data: &[u8]) {
    let mut h = [0u8; 5];
    h[0] = kind;
    h[1..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&h).await.expect("cabecera");
    w.write_all(data).await.expect("datos");
    w.flush().await.expect("flush");
}

/// Espera hasta `secs` a que `f` devuelva algo.
async fn until<T, F: Fn() -> Option<T>>(secs: u64, f: F) -> Option<T> {
    for _ in 0..secs * 20 {
        if let Some(v) = f() {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn del_streamer_al_espectador() {
    let (chat_addr, hls_addr, up_addr) = free_ports().await;
    let secret = b"una clave de al menos treinta y dos bytes".to_vec();
    let cfg = mestisexo_core::RepeaterConfig {
        listen: chat_addr,
        hls_listen: hls_addr,
        upstream: up_addr,
        max_viewers: 10,
        auth: None,
        hls_secret: Some(secret.clone()),
        events: None,
    };
    tokio::spawn(mestisexo_core::run_repeater(cfg));

    // El streamer entra, se presenta y empieza a mandar vídeo.
    let up = until(5, || std::net::TcpStream::connect(up_addr).ok())
        .await
        .expect("el repetidor debería estar escuchando");
    up.set_nonblocking(true).expect("nonblocking");
    let mut up = TcpStream::from_std(up).expect("tokio");
    let hello = serde_json::json!({
        "t": "hello", "key": "clavedeprueba", "history": [], "live": true, "codec": "avc1.42c01e",
    });
    frame(&mut up, 0, hello.to_string().as_bytes()).await;

    let frames = t::split_frames(&sample_mp4(6));
    for (kind, data) in &frames {
        let mut m = Vec::with_capacity(data.len() + 1);
        m.push(*kind);
        m.extend_from_slice(data);
        frame(&mut up, 1, &m).await;
    }

    // Un espectador abre el chat y debe recibir la ruta del directo.
    let ws = until(5, || std::net::TcpStream::connect(chat_addr).ok()).await.expect("chat");
    ws.set_nonblocking(true).expect("nonblocking");
    let ws = TcpStream::from_std(ws).expect("tokio");
    let url = format!("ws://{chat_addr}/?k=clavedeprueba");
    let (mut ws, _) = tokio_tungstenite::client_async(url, ws).await.expect("handshake");

    let mut path = None;
    for _ in 0..10 {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("sin respuesta del chat")
            .expect("conexión cerrada")
            .expect("mensaje");
        let Ok(v) = serde_json::from_str::<serde_json::Value>(msg.to_text().unwrap_or("")) else { continue };
        if matches!(v["t"].as_str(), Some("welcome") | Some("live")) {
            assert_eq!(v["live"].as_bool(), Some(true), "debería estar en directo: {v}");
            if let Some(p) = v["hls"].as_str() {
                path = Some(p.to_string());
                break;
            }
        }
    }
    let path = path.expect("el chat debe anunciar la ruta del directo");
    assert!(path.starts_with("/hls/") && path.ends_with("/live.m3u8"), "ruta rara: {path}");

    // Y con esa ruta, el vídeo se descarga por HTTP como lo haría el reproductor.
    let exp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600;
    let token = mestisexo_core::sign_token(&secret, exp).expect("firma");
    let base = format!("http://{hls_addr}");

    let r = reqwest::get(format!("{base}{path}?t={token}")).await.expect("playlist");
    assert_eq!(r.status(), 200);
    let m = r.text().await.expect("texto");
    assert!(m.contains("#EXT-X-PART:"), "sin partes:\n{m}");
    assert!(m.contains("#EXTINF:"), "sin segmentos:\n{m}");

    let dir = path.trim_end_matches("live.m3u8");
    let r = reqwest::get(format!("{base}{dir}init.mp4?t={token}")).await.expect("init");
    assert_eq!(r.status(), 200);
    assert!(!r.bytes().await.expect("cuerpo").is_empty());

    let seg = m.lines().find(|l| l.starts_with("seg/")).expect("hay un segmento");
    let r = reqwest::get(format!("{base}{dir}{seg}?t={token}")).await.expect("segmento");
    assert_eq!(r.status(), 200);
    assert!(!r.bytes().await.expect("cuerpo").is_empty());

    // Sin el token firmado no se entra, aunque se sepa la ruta exacta.
    assert_eq!(reqwest::get(format!("{base}{path}")).await.expect("sin token").status(), 403);

    // El streamer se va: el vídeo deja de servirse y el chat avisa.
    drop(up);
    let mut code = 0;
    for _ in 0..50 {
        code = reqwest::get(format!("{base}{path}?t={token}")).await.expect("playlist").status().as_u16();
        if code == 404 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(code, 404, "al irse el streamer la sesión debe desaparecer");
}
