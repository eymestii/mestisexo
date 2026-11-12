use crate::{Gate, Out, Pass, Shared, MAX_LAG};
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
/// Búfer de envío del sistema por conexión. Pequeño a propósito: con el valor
/// por defecto (hasta varios MB) el sistema acumula segundos de vídeo para un
/// espectador lento y se los entrega tarde; así el retraso se ve aquí y se descarta.
pub(crate) const SEND_BUFFER: usize = 128 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum In {
    Chat { text: String },
    Ping,
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
    let cb = |req: &Request, resp: Response| {
        let q = req.uri().query().unwrap_or("");
        let key = shared.lock().key.clone();
        if key.is_empty() || query_param(q, "k") != Some(key.as_str()) {
            return Err(reject(StatusCode::FORBIDDEN));
        }
        is_host = !shared.relay_mode && query_param(q, "h") == Some(shared.host_token.as_str());
        name = clean(&percent_decode(query_param(q, "n").unwrap_or("")), MAX_NAME);
        if !is_host && shared.lock().viewers >= shared.max_viewers {
            return Err(reject(StatusCode::SERVICE_UNAVAILABLE));
        }
        Ok(resp)
    };
    let cfg = WebSocketConfig { max_message_size: Some(4096), max_frame_size: Some(4096), ..Default::default() };
    let ws = tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::accept_hdr_async_with_config(stream, cb, Some(cfg)),
    )
    .await;
    let Ok(Ok(ws)) = ws else { return };
    if name.is_empty() {
        name = if is_host { "Streamer".into() } else { "Anónimo".into() };
    }

    let (mut sink, mut source) = ws.split();

    // Suscribirse antes de copiar el estado para no perder nada entre medias.
    let (mut rx, welcome, frames) = {
        let mut st = shared.lock();
        let rx = shared.tx.subscribe();
        if !is_host {
            st.viewers += 1;
        }
        (rx, st.welcome(is_host), st.cache.frames())
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
    for f in frames {
        if !ok { break; }
        ok = send!(Message::binary(f.to_vec()));
    }

    let mut last_msg = Instant::now() - MSG_INTERVAL;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut gate = Gate::default();
    while ok {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(Out::Text(t)) => ok = send!(Message::text(t.to_string())),
                Ok(Out::Bin(b, at)) => match gate.pass(b[0], at.elapsed() > MAX_LAG) {
                    Pass::Skip => {}
                    Pass::Send => ok = send!(Message::binary(b.to_vec())),
                    Pass::InitThenSend => {
                        let init = shared.lock().cache.init.clone();
                        if let Some(init) = init {
                            ok = send!(Message::binary(init.to_vec()));
                        }
                        if ok { ok = send!(Message::binary(b.to_vec())); }
                    }
                },
                Ok(Out::Kick) => break,
                Err(RecvError::Lagged(_)) => gate.lost(true),
                Err(RecvError::Closed) => break,
            },
            msg = source.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(In::Chat { text }) = serde_json::from_str::<In>(&t) {
                        let text = clean(&text, MAX_TEXT);
                        if !text.is_empty() && last_msg.elapsed() >= MSG_INTERVAL {
                            last_msg = Instant::now();
                            shared.chat_from(name.clone(), text, is_host);
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ping.tick() => ok = send!(Message::Ping(Vec::new())),
        }
    }

    if !is_host {
        let mut st = shared.lock();
        st.viewers = st.viewers.saturating_sub(1);
    }
    shared.viewers_changed();
}
