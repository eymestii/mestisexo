use crate::{Out, Shared};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

pub(crate) const MAX_NAME: usize = 24;
pub(crate) const MAX_TEXT: usize = 300;
const MSG_INTERVAL: Duration = Duration::from_millis(700);
/// Búfer de envío del sistema por conexión. Por aquí solo va chat, así que no
/// hace falta margen: cuanto más pequeño, antes se nota un espectador atascado.
pub(crate) const SEND_BUFFER: usize = 128 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Cuántos espectadores están teniendo problemas **ahora**. Cada conexión
/// avisa cuando entra y cuando sale de un estado malo, así que esto es el
/// estado actual, no un acumulado histórico.
#[derive(Default)]
pub(crate) struct QualityStats {
    pub poor_now: usize,
    pub avg_bandwidth: f32,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum In {
    Chat { text: String },
    Ping,
    #[serde(rename = "quality_hint")]
    QualityHint {
        bandwidth: u32,
        /// `true` mientras el espectador va mal; `false` en cuanto se recupera.
        poor: bool,
    },
}

pub(crate) async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let sh = shared.clone();
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            let _ = socket2::SockRef::from(&stream).set_send_buffer_size(SEND_BUFFER);
            handle(stream, sh).await;
        });
    }
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then_some(v)
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
}

fn reject(code: StatusCode) -> ErrorResponse {
    let mut r = ErrorResponse::new(None);
    *r.status_mut() = code;
    r
}

async fn handle(stream: TcpStream, shared: Arc<Shared>) {
    let mut name = String::new();
    let mut is_host = false;
    let mut ticket = String::new();
    let cb = |req: &Request, resp: Response| {
        let q = req.uri().query().unwrap_or("");
        // Con cuentas de Discord, el ticket es lo que da acceso: se entra aunque
        // el streamer aún no haya conectado y se espera a la señal.
        if !(shared.relay_mode && shared.auth.is_some()) {
            let key = shared.lock().key.clone();
            if key.is_empty() || query_param(q, "k") != Some(key.as_str()) {
                return Err(reject(StatusCode::FORBIDDEN));
            }
        }
        is_host = !shared.relay_mode && query_param(q, "h") == Some(shared.host_token.as_str());
        name = clean(&percent_decode(query_param(q, "n").unwrap_or("")), MAX_NAME);
        ticket = query_param(q, "t").unwrap_or("").to_string();
        if !is_host && shared.lock().viewers >= shared.max_viewers {
            return Err(reject(StatusCode::SERVICE_UNAVAILABLE));
        }
        Ok(resp)
    };
    let cfg = WebSocketConfig::default().max_message_size(Some(4096)).max_frame_size(Some(4096));
    let ws = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::accept_hdr_async_with_config(stream, cb, Some(cfg)),
    )
    .await;
    let Ok(Ok(mut ws)) = ws else { return };
    let mut sub = false;
    if let Some(auth) = shared.auth.as_ref().filter(|_| !is_host) {
        match crate::repeater::redeem(auth.internal, &ticket).await {
            Some(id) => {
                name = id.name;
                sub = id.sub;
            }
            None => {
                // Hay que iniciar sesión con Discord: la app lo hace en esta dirección y vuelve con un ticket.
                let m = serde_json::json!({ "t": "auth", "url": auth.public_url, "retry": !ticket.is_empty() });
                let _ = tokio::time::timeout(SEND_TIMEOUT, ws.send(Message::text(m.to_string()))).await;
                let _ = tokio::time::timeout(SEND_TIMEOUT, ws.close(None)).await;
                return;
            }
        }
    }
    if name.is_empty() {
        name = if is_host { "Streamer".into() } else { "Anónimo".into() };
    }

    let (mut sink, mut source) = ws.split();

    // Suscribirse antes de copiar el estado para no perder nada entre medias.
    let (mut rx, welcome) = {
        let mut st = shared.lock();
        let rx = shared.tx.subscribe();
        if !is_host {
            st.viewers += 1;
        }
        (rx, st.welcome(is_host))
    };
    shared.viewers_changed();

    // Todo envío tiene un plazo: si un espectador no acepta datos en ese tiempo
    // se le desconecta (su app reconecta y entra en el directo).
    macro_rules! send {
        ($m:expr) => {
            matches!(tokio::time::timeout(SEND_TIMEOUT, sink.send($m)).await, Ok(Ok(())))
        };
    }

    let mut ok = send!(Message::text(welcome.to_string()));

    let mut last_msg = Instant::now() - MSG_INTERVAL;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    // Si este espectador está contado como «va mal» ahora mismo.
    let mut poor_now = false;
    while ok {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(Out::Text(t)) => ok = send!(Message::text(t.to_string())),
                // El vídeo viaja por este canal hacia el repetidor (tunnel.rs),
                // no hacia los espectadores: aquí solo va el chat.
                Ok(Out::Bin(..)) => {}
                Ok(Out::Kick) => break,
                // El chat no se puede recuperar: se avisa y se sigue.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            msg = source.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(msg_in) = serde_json::from_str::<In>(&t) {
                        match msg_in {
                            In::Chat { text } => {
                                let text = clean(&text, MAX_TEXT);
                                if !text.is_empty() && last_msg.elapsed() >= MSG_INTERVAL {
                                    last_msg = Instant::now();
                                    shared.chat_from(name.clone(), text, is_host, sub);
                                }
                            }
                            In::Ping => {}
                            In::QualityHint { bandwidth, poor } if !is_host => {
                                {
                                    // Lock propio y sin vídeo detrás: cientos de
                                    // espectadores pueden informar a la vez sin
                                    // frenar el reparto de fragmentos.
                                    let mut q = shared.quality();
                                    // Solo cuenta el cambio de estado: así el total
                                    // refleja cuántos van mal ahora, y no se queda
                                    // contando a quien tuvo un bache hace una hora.
                                    if poor != poor_now {
                                        poor_now = poor;
                                        if poor {
                                            q.poor_now += 1;
                                        } else {
                                            q.poor_now = q.poor_now.saturating_sub(1);
                                        }
                                    }
                                    // Media móvil del ancho de banda de la audiencia.
                                    q.avg_bandwidth = if q.avg_bandwidth == 0.0 {
                                        bandwidth as f32
                                    } else {
                                        0.3 * bandwidth as f32 + 0.7 * q.avg_bandwidth
                                    };
                                }
                                // En el repetidor del VPS, el streamer es quien
                                // ve la píldora: hay que pasarle el aviso.
                                if shared.relay_mode {
                                    shared.to_upstream(serde_json::json!({
                                        "t": "quality_hint",
                                        "bandwidth": bandwidth,
                                        "poor": poor
                                    }));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ping.tick() => ok = send!(Message::Ping(Bytes::new())),
        }
    }

    if !is_host {
        {
            let mut st = shared.lock();
            st.viewers = st.viewers.saturating_sub(1);
        }
        // Quien se va deja de contar: si no, cerrar la pestaña con mala
        // conexión dejaría el contador alto para siempre.
        if poor_now {
            let mut q = shared.quality();
            q.poor_now = q.poor_now.saturating_sub(1);
        }
    }
    shared.viewers_changed();
}
