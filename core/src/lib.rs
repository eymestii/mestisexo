//! Núcleo del servidor de streaming.
//!
//! - Recibe la señal de OBS por RTMP (ffmpeg en modo `-listen`) y la remuxa a
//!   MP4 fragmentado sin recodificar.
//! - Envía una sola copia de los fragmentos al repetidor del VPS, que los
//!   empaqueta en LL-HLS (`hls.rs`) para que los espectadores los pidan por
//!   HTTP y un CDN pueda cachearlos.
//! - Prioriza el directo: si el VPS va con retraso se le descarta vídeo y se
//!   le salta al último keyframe en vez de mandarle lo atrasado.
//! - El WebSocket lleva solo chat, espectadores y calidad.
//! - El chat vive solo en memoria: se limita a `MAX_HISTORY` mensajes y se
//!   borra al terminar la sesión.

mod hls;
mod repeater;
mod tunnel;
mod video;
mod ws;

pub use hls::sign_token;
pub use repeater::{run as run_repeater, Auth as RepeaterAuth, RepeaterConfig};
pub use tunnel::Tunnel;

use bytes::Bytes;
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Puerto (solo loopback en el VPS) donde el repetidor espera al streamer.
pub const REPEATER_UPSTREAM_PORT: u16 = 8091;

pub(crate) const MAX_HISTORY: usize = 200;
pub(crate) const MAX_VIEWERS: usize = 100;

/// Primer byte de cada mensaje binario: INIT = segmento de inicialización,
/// FRAG_KEY = fragmento que empieza en keyframe, FRAG_DELTA = fragmento que
/// depende de los anteriores.
pub(crate) const INIT: u8 = 0;
pub(crate) const FRAG_KEY: u8 = 1;
pub(crate) const FRAG_DELTA: u8 = 2;

/// Retraso máximo de un fragmento en el salto PC → VPS antes de descartarlo y
/// saltar al directo.
pub(crate) const MAX_LAG: Duration = Duration::from_millis(1000);

/// Tope de fragmentos guardados desde el último keyframe (≈10 s con fragmentos de 0,25 s).
const MAX_GOP_FRAGS: usize = 40;

#[derive(Clone)]
pub(crate) enum Out {
    Text(Arc<str>),
    /// Vídeo hacia el repetidor del VPS (`tunnel.rs`). Los espectadores ya no
    /// lo reciben por aquí: lo piden por HTTP. `Instant` es el momento en que
    /// se generó, para descartar lo que llegaría tarde si el enlace se atasca.
    Bin(Bytes, Instant),
    /// Cierra todas las conexiones de espectadores (la clave ha cambiado).
    Kick,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ChatMsg {
    pub id: u64,
    pub name: String,
    pub text: String,
    pub ts: u64,
    pub host: bool,
    /// Suscriptor (insignia en el chat).
    #[serde(default)]
    pub sub: bool,
}

/// Lo justo para que quien entra pueda decodificar al momento: el init y los
/// fragmentos desde el último keyframe. Nada más antiguo.
#[derive(Default)]
pub(crate) struct Cache {
    pub init: Option<Bytes>,
    pub gop: Vec<Bytes>,
}

impl Cache {
    pub fn push(&mut self, b: &Bytes) {
        match b.first() {
            Some(&INIT) => {
                self.init = Some(b.clone());
                self.gop.clear();
            }
            Some(&FRAG_KEY) => {
                self.gop.clear();
                self.gop.push(b.clone());
            }
            _ if !self.gop.is_empty() && self.gop.len() < MAX_GOP_FRAGS => self.gop.push(b.clone()),
            _ => self.gop.clear(),
        }
    }

    pub fn clear(&mut self) {
        self.init = None;
        self.gop.clear();
    }

    /// Init seguido del GOP actual.
    pub fn frames(&self) -> Vec<Bytes> {
        self.init.iter().chain(self.gop.iter()).cloned().collect()
    }
}

/// Decide, para una conexión, qué fragmentos se envían tras perder datos: los
/// que dependen de fragmentos descartados no se pueden decodificar, así que
/// se espera al siguiente keyframe.
#[derive(Default)]
pub(crate) struct Gate {
    resync: bool,
    need_init: bool,
}

pub(crate) enum Pass {
    Skip,
    Send,
    InitThenSend,
}

impl Gate {
    /// Se han perdido mensajes; `init_too` si entre ellos pudo ir un init.
    pub fn lost(&mut self, init_too: bool) {
        self.resync = true;
        self.need_init |= init_too;
    }

    /// `late`: el destino va con retraso y hay que descartar.
    pub fn pass(&mut self, kind: u8, late: bool) -> Pass {
        if kind == INIT {
            self.need_init = false;
            return Pass::Send;
        }
        if late {
            self.resync = true;
            return Pass::Skip;
        }
        if self.resync {
            if kind != FRAG_KEY {
                return Pass::Skip;
            }
            self.resync = false;
            if std::mem::take(&mut self.need_init) {
                return Pass::InitThenSend;
            }
        }
        Pass::Send
    }
}

#[derive(Default)]
pub(crate) struct State {
    /// Clave que deben presentar los espectadores (en el repetidor la envía el streamer).
    pub key: String,
    /// Códec MSE de la señal actual. `Some` equivale a estar en directo.
    pub codec: Option<String>,
    /// Init y GOP actual. Solo en el streamer: es lo que se reenvía al
    /// repetidor cuando el enlace se recupera, para que pueda empezar una
    /// sesión HLS nueva sin esperar al siguiente keyframe de OBS.
    pub cache: Cache,
    /// En el repetidor, ruta de la playlist de la señal actual.
    pub hls: Option<String>,
    pub chat: VecDeque<ChatMsg>,
    pub next_id: u64,
    /// Espectadores conectados directamente a este proceso.
    pub viewers: usize,
    /// Espectadores conectados a través del repetidor (solo en el streamer).
    pub relay_viewers: usize,
    /// Total que comunica el streamer (solo en el repetidor).
    pub upstream_total: Option<usize>,
    pub status: String,
    pub tunnel_status: Option<String>,
    pub tunnel_up: Option<bool>,
    /// Momento (ms) en que empezó la señal actual de OBS.
    pub live_since: Option<u64>,
    pub bytes_total: u64,
    pub frags_total: u64,
    pub last_data_ms: u64,
}

impl State {
    pub fn live(&self) -> bool {
        self.codec.is_some()
    }

    pub fn total_viewers(&self) -> usize {
        self.upstream_total.unwrap_or(self.viewers + self.relay_viewers)
    }

    pub fn welcome(&self, host: bool) -> serde_json::Value {
        serde_json::json!({
            "t": "welcome",
            "history": &self.chat,
            "live": self.live(),
            "hls": self.hls,
            "viewers": self.total_viewers(),
            "host": host,
        })
    }
}

pub(crate) struct Shared {
    pub host_token: String,
    pub max_viewers: usize,
    /// `true` en el repetidor del VPS: el chat y el recuento van al streamer.
    pub relay_mode: bool,
    pub tx: broadcast::Sender<Out>,
    pub state: Mutex<State>,
    /// Aparte de `state` a propósito: cada espectador actualiza esto por su
    /// cuenta cada 2 s, así que con el runtime multihilo mantenerlo en el lock
    /// grande haría que cientos de tareas compitieran por él sin necesidad.
    pub quality: Mutex<crate::ws::QualityStats>,
    /// En el repetidor, canal hacia el streamer mientras está conectado.
    pub upstream: Mutex<Option<mpsc::Sender<String>>>,
    /// En el repetidor, cuentas de Discord obligatorias para los espectadores.
    pub auth: Option<repeater::Auth>,
    /// En el repetidor, origen LL-HLS del que tiran los espectadores.
    pub hls: Option<Arc<hls::Hls>>,
}

impl Shared {
    pub fn new(key: String, max_viewers: usize, relay_mode: bool, status: &str, auth: Option<repeater::Auth>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        Arc::new(Shared {
            host_token: random_token(32),
            max_viewers,
            relay_mode,
            tx,
            state: Mutex::new(State { key, status: status.into(), ..Default::default() }),
            quality: Mutex::new(Default::default()),
            upstream: Mutex::new(None),
            auth,
            hls: None,
        })
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn quality(&self) -> std::sync::MutexGuard<'_, crate::ws::QualityStats> {
        self.quality.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn send_json(&self, v: serde_json::Value) {
        let _ = self.tx.send(Out::Text(v.to_string().into()));
    }

    pub fn set_status(&self, s: impl Into<String>) {
        self.lock().status = s.into();
    }

    pub fn push_chat(&self, name: String, text: String, host: bool, sub: bool) {
        let mut st = self.lock();
        st.next_id += 1;
        let msg = ChatMsg { id: st.next_id, name, text, ts: now_ms(), host, sub };
        let json = serde_json::json!({ "t": "chat", "m": &msg });
        st.chat.push_back(msg);
        while st.chat.len() > MAX_HISTORY {
            st.chat.pop_front();
        }
        // Se envía con el lock tomado para mantener el orden respecto al historial.
        self.send_json(json);
    }

    /// Mensaje de chat de un espectador conectado a este proceso.
    pub fn chat_from(&self, name: String, text: String, host: bool, sub: bool) {
        if self.relay_mode {
            self.to_upstream(serde_json::json!({ "t": "chat", "name": name, "text": text, "sub": sub }));
        } else {
            self.push_chat(name, text, host, sub);
        }
    }

    pub fn viewers_changed(&self) {
        if self.relay_mode {
            let n = self.lock().viewers;
            self.to_upstream(serde_json::json!({ "t": "viewers", "n": n }));
        } else {
            let n = self.lock().total_viewers();
            self.send_json(serde_json::json!({ "t": "viewers", "n": n }));
        }
    }

    fn to_upstream(&self, v: serde_json::Value) {
        if let Some(tx) = self.upstream.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.try_send(v.to_string());
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Acceso al empaquetado y a la firma para las pruebas. No forma parte de la
/// interfaz del servidor: solo existe para poder comprobar el empaquetado
/// contra vídeo real de ffmpeg.
#[doc(hidden)]
pub mod testing {
    pub use crate::hls::{token_ok, Packager};

    pub const INIT: u8 = crate::INIT;
    pub const FRAG_KEY: u8 = crate::FRAG_KEY;
    pub const FRAG_DELTA: u8 = crate::FRAG_DELTA;

    /// Parte un MP4 fragmentado en los mismos mensajes que produce `video.rs`:
    /// (tipo, datos sin el byte de tipo).
    pub fn split_frames(mp4: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut init: Vec<u8> = Vec::new();
        let mut frag: Vec<u8> = Vec::new();
        let mut i = 0;
        while let Some((kind, body, next)) = crate::video::box_at(mp4, i) {
            match kind {
                b"ftyp" => init = body.to_vec(),
                b"moov" => {
                    init.extend_from_slice(body);
                    out.push((INIT, std::mem::take(&mut init)));
                }
                b"moof" => {
                    frag = body.to_vec();
                    out.push((if crate::video::starts_with_keyframe(body) { FRAG_KEY } else { FRAG_DELTA }, Vec::new()));
                }
                b"mdat" if !frag.is_empty() => {
                    frag.extend_from_slice(body);
                    if let Some(last) = out.last_mut() {
                        last.1 = std::mem::take(&mut frag);
                    }
                }
                _ => {}
            }
            i = next;
        }
        out.retain(|(_, d)| !d.is_empty());
        out
    }
}

pub fn random_token(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

pub(crate) fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 64 && k.chars().all(|c| c.is_ascii_alphanumeric())
}

pub struct Config {
    /// Puerto TCP donde escuchan los espectadores (WebSocket).
    pub ws_port: u16,
    /// Puerto RTMP local al que se conecta OBS.
    pub rtmp_port: u16,
    /// Ruta o nombre del ejecutable de ffmpeg.
    pub ffmpeg: String,
    /// Enviar la señal a un repetidor en un VPS (por SSH) que la reparte.
    pub tunnel: Option<Tunnel>,
    /// Clave fija para espectadores; si es `None` se genera una nueva.
    pub viewer_key: Option<String>,
    /// Clave de stream fija para OBS; si es `None` se genera una nueva.
    pub stream_key: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Status {
    pub live: bool,
    pub viewers: usize,
    pub chat_messages: usize,
    pub status: String,
    pub tunnel: Option<String>,
    pub tunnel_up: Option<bool>,
    pub live_since: Option<u64>,
    pub bytes_total: u64,
    pub frags_total: u64,
    pub last_data_ms: u64,
    pub now_ms: u64,
    /// Espectadores con problemas de conexión ahora mismo, y ancho de banda
    /// medio de la audiencia que ha informado.
    pub quality_poor_now: usize,
    pub quality_avg_bandwidth: f32,
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
    pub fn key(&self) -> String {
        self.shared.lock().key.clone()
    }

    pub fn host_token(&self) -> &str {
        &self.shared.host_token
    }

    pub fn status(&self) -> Status {
        // El lock de calidad se toma y se suelta antes del de estado: así nunca
        // se mantienen los dos a la vez y no hay orden de bloqueo que respetar.
        let (quality_poor_now, quality_avg_bandwidth) = {
            let q = self.shared.quality();
            (q.poor_now, q.avg_bandwidth)
        };
        let st = self.shared.lock();
        Status {
            live: st.live(),
            viewers: st.total_viewers(),
            chat_messages: st.chat.len(),
            status: st.status.clone(),
            tunnel: st.tunnel_status.clone(),
            tunnel_up: st.tunnel_up,
            live_since: st.live_since,
            bytes_total: st.bytes_total,
            frags_total: st.frags_total,
            last_data_ms: st.last_data_ms,
            now_ms: now_ms(),
            quality_poor_now,
            quality_avg_bandwidth,
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
        // Dar un momento a que el aviso salga hacia los espectadores y el repetidor.
        tokio::time::sleep(Duration::from_millis(150)).await;
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
    // Solo para la propia app: los espectadores entran por el repetidor del VPS.
    let listener = TcpListener::bind(("127.0.0.1", cfg.ws_port)).await.map_err(|e| {
        io::Error::new(e.kind(), format!("No se pudo abrir el puerto {}: {e}", cfg.ws_port))
    })?;
    let ws_port = listener.local_addr()?.port();

    let key = cfg.viewer_key.clone().unwrap_or_else(|| random_token(10));
    let shared = Shared::new(key, MAX_VIEWERS, false, "Esperando a OBS…", None);

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
        tasks.push(tokio::spawn(tunnel::run(shared.clone(), t)));
    }

    Ok(Handle { shared, tasks, ws_port, rtmp_server, stream_key })
}
