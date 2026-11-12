//! Repetidor para un VPS: recibe una sola copia de la señal del streamer y la
//! reparte a todos los espectadores. Así la subida del streamer solo necesita
//! el bitrate de OBS, tenga los espectadores que tenga.
//!
//! El streamer llega por `ssh -W` al puerto de subida (solo loopback) y habla
//! un protocolo de tramas: `[tipo u8][longitud u32][datos]`.

use crate::{now_ms, ws, ChatMsg, Out, Shared};
use serde::Deserialize;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

/// JSON (eventos, chat, estado).
pub(crate) const T_JSON: u8 = 0;
/// Mensaje de vídeo tal cual se envía a los espectadores (byte de tipo + MP4).
pub(crate) const T_MEDIA: u8 = 1;
/// Del repetidor al streamer: número de mensajes de vídeo recibidos (u64).
pub(crate) const T_ACK: u8 = 2;

const MAX_FRAME: usize = 64 * 1024 * 1024;

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, kind: u8, data: &[u8]) -> io::Result<()> {
    let mut h = [0u8; 5];
    h[0] = kind;
    h[1..].copy_from_slice(&(data.len() as u32).to_be_bytes());
    w.write_all(&h).await?;
    w.write_all(data).await?;
    w.flush().await
}

pub(crate) async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    r.read_exact(&mut h).await?;
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "trama demasiado grande"));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok((h[0], buf))
}

pub struct RepeaterConfig {
    /// Dirección pública para los espectadores, p. ej. `127.0.0.1:8090` (detrás de Caddy).
    pub listen: SocketAddr,
    /// Dirección local donde llega el streamer por SSH, p. ej. `127.0.0.1:8091`.
    pub upstream: SocketAddr,
    pub max_viewers: usize,
    /// Si se indica, los espectadores tienen que iniciar sesión con Discord.
    pub auth: Option<Auth>,
}

/// Servicio de cuentas (`mestisexo-auth`) en el mismo VPS.
#[derive(Clone)]
pub struct Auth {
    /// Dirección interna donde se canjean los tickets, p. ej. `127.0.0.1:8093`.
    pub internal: SocketAddr,
    /// Dirección pública HTTPS que usa la app para iniciar sesión.
    pub public_url: String,
}

/// Identidad de un espectador ya comprobada por el servicio de cuentas.
pub(crate) struct Identity {
    pub name: String,
    pub sub: bool,
}

/// Canjea el ticket de un solo uso que la app obtuvo del servicio de cuentas.
pub(crate) async fn redeem(addr: SocketAddr, ticket: &str) -> Option<Identity> {
    if ticket.is_empty() || ticket.len() > 64 || !ticket.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let work = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        let req = format!("GET /internal/redeem?t={ticket} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        (&mut s).take(16 * 1024).read_to_end(&mut buf).await.ok()?;
        let text = String::from_utf8_lossy(&buf);
        let (head, body) = text.split_once("\r\n\r\n")?;
        if !head.starts_with("HTTP/1.1 200") {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(body).ok()?;
        let name = ws::clean(v["name"].as_str()?, ws::MAX_NAME);
        Some(Identity { name: if name.is_empty() { "Usuario".into() } else { name }, sub: v["sub"].as_bool().unwrap_or(false) })
    };
    tokio::time::timeout(Duration::from_secs(5), work).await.ok().flatten()
}

pub async fn run(cfg: RepeaterConfig) -> io::Result<()> {
    let public = TcpListener::bind(cfg.listen).await?;
    let upstream = TcpListener::bind(cfg.upstream).await?;
    let shared = Shared::new(String::new(), cfg.max_viewers, true, "Esperando al streamer…", cfg.auth.clone());
    tokio::spawn(ws::serve(public, shared.clone()));
    eprintln!("Repetidor: espectadores en {}, streamer en {}", cfg.listen, cfg.upstream);

    // Solo un streamer a la vez: si entra uno nuevo (p. ej. tras un corte), sustituye al anterior.
    let mut current: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let (stream, addr) = match upstream.accept().await {
            Ok(c) => c,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if let Some(old) = current.take() {
            old.abort();
            let _ = old.await;
        }
        eprintln!("Streamer conectado ({addr})");
        let sh = shared.clone();
        current = Some(tokio::spawn(async move {
            let _ = upstream_session(&sh, stream).await;
            eprintln!("Streamer desconectado");
            upstream_gone(&sh);
        }));
    }
}

#[derive(Deserialize)]
struct Hello {
    key: String,
    #[serde(default)]
    history: Vec<ChatMsg>,
    live: bool,
    codec: Option<String>,
    viewers: Option<usize>,
}

async fn upstream_session(shared: &Arc<Shared>, stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let (r, w) = stream.into_split();
    let mut r = BufReader::with_capacity(256 * 1024, r);
    let mut w = BufWriter::new(w);

    let (json_tx, mut json_rx) = mpsc::channel::<String>(256);
    let (ack_tx, mut ack_rx) = watch::channel(0u64);
    *shared.upstream.lock().unwrap_or_else(|e| e.into_inner()) = Some(json_tx);

    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                m = json_rx.recv() => match m {
                    Some(m) => if write_frame(&mut w, T_JSON, m.as_bytes()).await.is_err() { break },
                    None => break,
                },
                c = ack_rx.changed() => {
                    if c.is_err() { break; }
                    let n = *ack_rx.borrow_and_update();
                    if write_frame(&mut w, T_ACK, &n.to_be_bytes()).await.is_err() { break; }
                }
            }
        }
    });
    let _guard = AbortOnDrop(writer);

    let mut received = 0u64;
    let mut hello_seen = false;
    loop {
        let (kind, data) = read_frame(&mut r).await?;
        match kind {
            T_MEDIA if hello_seen && !data.is_empty() => {
                let f = Arc::new(data);
                let mut st = shared.lock();
                st.cache.push(&f);
                st.bytes_total += f.len() as u64;
                st.last_data_ms = now_ms();
                let _ = shared.tx.send(Out::Bin(f, Instant::now()));
                drop(st);
                received += 1;
                let _ = ack_tx.send(received);
            }
            T_JSON => {
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) else { continue };
                if v["t"] == "hello" {
                    let Ok(h) = serde_json::from_value::<Hello>(v) else { continue };
                    if !crate::valid_key(&h.key) {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "clave inválida"));
                    }
                    on_hello(shared, h);
                    hello_seen = true;
                    // Que el streamer sepa cuántos espectadores hay aquí.
                    shared.viewers_changed();
                } else if hello_seen {
                    on_event(shared, &v, &data);
                }
            }
            _ => {}
        }
    }
}

fn on_hello(shared: &Shared, h: Hello) {
    let mut st = shared.lock();
    if !st.key.is_empty() && st.key != h.key {
        // Clave nueva: los que entraron con la anterior quedan fuera.
        let _ = shared.tx.send(Out::Kick);
    }
    st.key = h.key;
    st.chat = h.history.into_iter().collect();
    st.upstream_total = h.viewers;
    st.cache.clear();
    st.codec = h.codec.filter(|_| h.live);
    st.status = "Streamer conectado".into();
    // El init y el GOP actual llegan justo después como vídeo normal.
    if let Some(codec) = &st.codec {
        shared.send_json(serde_json::json!({ "t": "live", "codec": codec }));
    }
}

fn on_event(shared: &Shared, v: &serde_json::Value, raw: &[u8]) {
    let mut st = shared.lock();
    match v["t"].as_str() {
        Some("chat") => {
            if let Ok(m) = serde_json::from_value::<ChatMsg>(v["m"].clone()) {
                st.chat.push_back(m);
                while st.chat.len() > crate::MAX_HISTORY {
                    st.chat.pop_front();
                }
            }
        }
        Some("clear") => st.chat.clear(),
        Some("viewers") => st.upstream_total = v["n"].as_u64().map(|n| n as usize),
        Some("live") => {
            st.codec = v["codec"].as_str().map(String::from);
            st.cache.clear();
        }
        Some("offline") => {
            st.codec = None;
            st.cache.clear();
        }
        Some("end") => {
            st.codec = None;
            st.cache.clear();
            st.chat.clear();
            st.key.clear();
            let _ = shared.tx.send(Out::Text(String::from_utf8_lossy(raw).into()));
            let _ = shared.tx.send(Out::Kick);
            return;
        }
        _ => {}
    }
    // Se reenvía con el lock tomado, igual que en el streamer, para mantener el orden.
    let _ = shared.tx.send(Out::Text(String::from_utf8_lossy(raw).into()));
}

fn upstream_gone(shared: &Shared) {
    *shared.upstream.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let mut st = shared.lock();
    st.upstream_total = None;
    st.status = "Esperando al streamer…".into();
    if st.live() {
        st.codec = None;
        st.cache.clear();
        // La clave y el chat se mantienen: si el streamer vuelve, se sigue igual.
        shared.send_json(serde_json::json!({ "t": "offline" }));
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
