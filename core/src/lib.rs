//! Núcleo del servidor de streaming local.
//!
//! - Recibe la señal de OBS por RTMP (ffmpeg en modo `-listen`) y la remuxa a
//!   MP4 fragmentado sin recodificar.
//! - Reparte los fragmentos y el chat a los espectadores por WebSocket.
//! - El chat vive solo en memoria: se limita a `MAX_HISTORY` mensajes y se
//!   borra al terminar la sesión.

mod tunnel;
mod video;
mod ws;

pub use tunnel::Tunnel;

use rand::{distributions::Alphanumeric, Rng};
use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

pub(crate) const MAX_HISTORY: usize = 200;
pub(crate) const MAX_VIEWERS: usize = 100;

pub struct Config {
    /// Puerto TCP donde escuchan los espectadores (WebSocket).
    pub ws_port: u16,
    /// Puerto RTMP local al que se conecta OBS.
    pub rtmp_port: u16,
    /// Ruta o nombre del ejecutable de ffmpeg.
    pub ffmpeg: String,
    /// Publicar a través de un VPS con un túnel SSH inverso.
    pub tunnel: Option<Tunnel>,
    /// Clave fija para espectadores; si es `None` se genera una nueva.
    pub viewer_key: Option<String>,
    /// Clave de stream fija para OBS; si es `None` se genera una nueva.
    pub stream_key: Option<String>,
}

#[derive(Clone)]
pub(crate) enum Out {
    Text(Arc<str>),
    Bin(Arc<Vec<u8>>),
}

#[derive(Clone, Serialize)]
pub(crate) struct ChatMsg {
    pub id: u64,
    pub name: String,
    pub text: String,
    pub ts: u64,
    pub host: bool,
}

#[derive(Default)]
pub(crate) struct State {
    /// Códec MSE + segmento de inicialización (con byte de tipo 0 delante).
    pub init: Option<(String, Arc<Vec<u8>>)>,
    /// Último fragmento (empieza siempre en keyframe), para que quien entra vea algo al momento.
    pub last_frag: Option<Arc<Vec<u8>>>,
    pub chat: VecDeque<ChatMsg>,
    pub next_id: u64,
    pub viewers: usize,
    pub status: String,
    pub tunnel_status: Option<String>,
}

pub(crate) struct Shared {
    pub key: String,
    pub host_token: String,
    pub tx: broadcast::Sender<Out>,
    pub state: Mutex<State>,
}

impl Shared {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn send_json(&self, v: serde_json::Value) {
        let _ = self.tx.send(Out::Text(v.to_string().into()));
    }

    pub fn set_status(&self, s: impl Into<String>) {
        self.lock().status = s.into();
    }

    pub fn push_chat(&self, name: String, text: String, host: bool) {
        let mut st = self.lock();
        st.next_id += 1;
        let msg = ChatMsg { id: st.next_id, name, text, ts: now_ms(), host };
        let json = serde_json::json!({ "t": "chat", "m": &msg });
        st.chat.push_back(msg);
        while st.chat.len() > MAX_HISTORY {
            st.chat.pop_front();
        }
        // Se envía con el lock tomado para mantener el orden respecto al historial.
        self.send_json(json);
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn random_token(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

#[derive(Serialize, Clone)]
pub struct Status {
    pub live: bool,
    pub viewers: usize,
    pub chat_messages: usize,
    pub status: String,
    pub tunnel: Option<String>,
}

/// Sesión de stream activa. Al soltarla (o llamar a `stop`) se cierra todo y
/// el chat desaparece.
pub struct Handle {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
    pub ws_port: u16,
    pub rtmp_server: String,
    pub stream_key: String,
}

impl Handle {
    pub fn key(&self) -> &str {
        &self.shared.key
    }

    pub fn host_token(&self) -> &str {
        &self.shared.host_token
    }

    pub fn status(&self) -> Status {
        let st = self.shared.lock();
        Status {
            live: st.init.is_some(),
            viewers: st.viewers,
            chat_messages: st.chat.len(),
            status: st.status.clone(),
            tunnel: st.tunnel_status.clone(),
        }
    }

    pub fn clear_chat(&self) {
        let mut st = self.shared.lock();
        st.chat.clear();
        self.shared.send_json(serde_json::json!({ "t": "clear" }));
    }

    /// Detiene la sesión y espera a que se liberen los puertos.
    pub async fn stop(mut self) {
        self.shared.send_json(serde_json::json!({ "t": "end" }));
        let tasks = std::mem::take(&mut self.tasks);
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        self.shared.lock().chat.clear();
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Arranca una sesión. Debe llamarse dentro de un runtime de Tokio.
pub async fn start(cfg: Config) -> io::Result<Handle> {
    let listener = TcpListener::bind(("0.0.0.0", cfg.ws_port)).await.map_err(|e| {
        io::Error::new(e.kind(), format!("No se pudo abrir el puerto {}: {e}", cfg.ws_port))
    })?;
    let ws_port = listener.local_addr()?.port();

    let (tx, _) = broadcast::channel(64);
    let shared = Arc::new(Shared {
        key: cfg.viewer_key.clone().unwrap_or_else(|| random_token(10)),
        host_token: random_token(32),
        tx,
        state: Mutex::new(State { status: "Esperando a OBS…".into(), ..Default::default() }),
    });

    // RTMP solo en loopback: ffmpeg no valida la clave de stream, así que no
    // se expone a la red. OBS debe correr en el mismo equipo.
    let stream_key = cfg.stream_key.clone().unwrap_or_else(|| random_token(16));
    let rtmp_server = format!("rtmp://127.0.0.1:{}/live", cfg.rtmp_port);
    let rtmp_url = format!("{rtmp_server}/{stream_key}");

    let mut tasks = vec![
        tokio::spawn(ws::serve(listener, shared.clone())),
        tokio::spawn(video::run(shared.clone(), cfg.ffmpeg, rtmp_url)),
    ];
    if let Some(t) = cfg.tunnel {
        tasks.push(tokio::spawn(tunnel::run(shared.clone(), t, ws_port)));
    }

    Ok(Handle { shared, tasks, ws_port, rtmp_server, stream_key })
}

/// Relé TCP local para el modo espectador: el webview solo puede abrir
/// `ws://` hacia loopback (contenido mixto), así que conecta a
/// `127.0.0.1:<puerto devuelto>` y el relé reenvía a `remote` ("host:puerto").
pub async fn relay(remote: String) -> io::Result<(u16, JoinHandle<()>)> {
    // Comprobar que el destino responde antes de dar el puerto al webview.
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect(remote.as_str()),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "El streamer no responde"))??;
    drop(probe);

    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        while let Ok((mut local, _)) = listener.accept().await {
            let remote = remote.clone();
            tokio::spawn(async move {
                if let Ok(mut up) = tokio::net::TcpStream::connect(remote.as_str()).await {
                    let _ = up.set_nodelay(true);
                    let _ = local.set_nodelay(true);
                    let _ = tokio::io::copy_bidirectional(&mut local, &mut up).await;
                }
            });
        }
    });
    Ok((port, task))
}

/// IPv4 de Tailscale (rango 100.64.0.0/10) si está instalado y conectado.
pub fn tailscale_ip() -> Option<IpAddr> {
    let mut cmd = std::process::Command::new("tailscale");
    cmd.args(["ip", "-4"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok().filter(|o| o.status.success())?;
    let ip: std::net::Ipv4Addr = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()?;
    let o = ip.octets();
    (o[0] == 100 && (o[1] & 0xC0) == 64).then_some(IpAddr::V4(ip))
}

/// IP de la interfaz de red con salida por defecto (no envía tráfico).
pub fn local_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}
