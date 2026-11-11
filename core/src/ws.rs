use crate::{Out, Shared, MAX_VIEWERS};
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

const MAX_NAME: usize = 24;
const MAX_TEXT: usize = 300;
const MSG_INTERVAL: Duration = Duration::from_millis(700);

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum In {
    Chat { text: String },
    Ping,
}

pub async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let sh = shared.clone();
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
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

fn clean(s: &str, max: usize) -> String {
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
        if query_param(q, "k") != Some(shared.key.as_str()) {
            return Err(reject(StatusCode::FORBIDDEN));
        }
        is_host = query_param(q, "h") == Some(shared.host_token.as_str());
        name = clean(&percent_decode(query_param(q, "n").unwrap_or("")), MAX_NAME);
        if !is_host && shared.lock().viewers >= MAX_VIEWERS {
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
    let (mut rx, welcome, init, frag) = {
        let mut st = shared.lock();
        let rx = shared.tx.subscribe();
        if !is_host {
            st.viewers += 1;
        }
        let welcome = serde_json::json!({
            "t": "welcome",
            "history": &st.chat,
            "live": st.init.is_some(),
            "codec": st.init.as_ref().map(|i| i.0.clone()),
            "viewers": st.viewers,
            "host": is_host,
        });
        (rx, welcome, st.init.clone(), st.last_frag.clone())
    };
    broadcast_viewers(&shared);

    let mut ok = sink.send(Message::text(welcome.to_string())).await.is_ok();
    if ok {
        if let Some((_, init)) = init {
            ok = sink.send(Message::binary(init.to_vec())).await.is_ok();
            if let (true, Some(f)) = (ok, frag) {
                ok = sink.send(Message::binary(f.to_vec())).await.is_ok();
            }
        }
    }

    let mut last_msg = Instant::now() - MSG_INTERVAL;
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let mut need_init = false;
    while ok {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(Out::Text(t)) => ok = sink.send(Message::text(t.to_string())).await.is_ok(),
                Ok(Out::Bin(b)) => {
                    // Tras perder datos, reenviar el init y esperar un fragmento nuevo.
                    if need_init {
                        if b[0] != 1 { need_init = false; }
                        else {
                            let init = shared.lock().init.clone();
                            let Some((_, init)) = init else { continue };
                            ok = sink.send(Message::binary(init.to_vec())).await.is_ok();
                            need_init = false;
                        }
                    }
                    if ok { ok = sink.send(Message::binary(b.to_vec())).await.is_ok(); }
                }
                Err(RecvError::Lagged(_)) => need_init = true,
                Err(RecvError::Closed) => break,
            },
            msg = source.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    if let Ok(In::Chat { text }) = serde_json::from_str::<In>(&t) {
                        let text = clean(&text, MAX_TEXT);
                        if !text.is_empty() && last_msg.elapsed() >= MSG_INTERVAL {
                            last_msg = Instant::now();
                            shared.push_chat(name.clone(), text, is_host);
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
            _ = ping.tick() => ok = sink.send(Message::Ping(Vec::new())).await.is_ok(),
        }
    }

    if !is_host {
        let mut st = shared.lock();
        st.viewers = st.viewers.saturating_sub(1);
    }
    broadcast_viewers(&shared);
}

fn broadcast_viewers(shared: &Shared) {
    let n = shared.lock().viewers;
    shared.send_json(serde_json::json!({ "t": "viewers", "n": n }));
}
