//! Origen LL-HLS: convierte los fragmentos que llegan del streamer en una
//! playlist y unos ficheros que cualquier CDN puede cachear.
//!
//! No hace falta un empaquetador aparte: `video.rs` ya le pide a ffmpeg
//! fragmentos de 0,25 s (`-frag_duration 250000`) y marca cuáles empiezan en
//! keyframe, que es exactamente lo que LL-HLS llama «partial segment». Aquí
//! solo se agrupan: cada fragmento es un `EXT-X-PART` y los fragmentos entre
//! keyframes forman un segmento completo.
//!
//! Lo que el espectador descarga (segmentos, partes, init) no cambia nunca
//! para una misma URL, así que se puede cachear para siempre. Lo único que
//! caduca es la playlist.
//!
//! El permiso viaja en la playlist (`?t=`, el token que firma el servicio de
//! cuentas). Al servirla, el origen deja además una cookie atada a
//! `/hls/<sesión>/`: las URLs de media quedan limpias e idénticas para todos
//! los espectadores, y el navegador manda la cookie solo al pedirlas. Así
//! funciona igual en Chrome, Firefox y Safari nativo, y un CDN puede cachear
//! los trozos sin mirar el token.

use crate::video::{be32, children};
use crate::{FRAG_KEY, INIT};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use bytes::{Bytes, BytesMut};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Duración nominal de cada parte: la de los fragmentos que genera ffmpeg.
const PART_TARGET: f64 = 0.25;
/// Partes normales que se miden antes de fijar `PART-TARGET` (unos 2 s, para
/// ver todas las duraciones que alterna ffmpeg). Si luego llega una parte más
/// larga, solo su segmento se queda sin partes.
const PART_SAMPLES: usize = 8;
/// Antes de fijar `PART-TARGET`, una parte que pase de esto es anormal (la del
/// arranque del encoder llega a durar 0,7 s) y no cuenta para medirlo.
const PART_ODD: f64 = PART_TARGET * 1.5;
/// Un segmento se cierra en el primer keyframe a partir de esta duración.
const SEG_TARGET: f64 = 2.0;
/// `TARGETDURATION` mínimo. Se anuncia con margen desde la primera playlist
/// porque el primer segmento de cada directo dura algo más de 3 s y el valor
/// no puede cambiar durante la emisión.
const TARGET_MIN: f64 = 4.0;
/// Segmentos que se mantienen descargables. Las partes de los más antiguos
/// siguen disponibles aunque ya no aparezcan en la playlist, como pide la
/// especificación (tres duraciones de segmento desde el borde del directo).
const KEEP_SEGMENTS: usize = 6;
/// Segmentos del final de la playlist que publican sus partes.
const PART_SEGMENTS: usize = 3;
/// Tope de espera de una recarga bloqueante o de una parte aún no publicada.
const BLOCK_MAX: Duration = Duration::from_secs(5);

/// Una parte: un fragmento tal cual salió de ffmpeg, sin el byte de tipo.
struct Part {
    data: Bytes,
    dur: f64,
    /// Empieza en keyframe, así que se puede decodificar por sí sola.
    independent: bool,
}

struct Seg {
    msn: u64,
    parts: Vec<Part>,
    dur: f64,
    /// Tiene una parte más larga que `PART-TARGET`: se publica solo entero,
    /// porque anunciar sus partes rompería la playlist.
    no_parts: bool,
    /// Concatenación de las partes, una vez cerrado el segmento.
    data: Option<Bytes>,
}

#[derive(Default)]
struct Inner {
    /// Identifica la señal actual. Va en la ruta, así que al reiniciar el
    /// directo las URLs son nuevas y nada cacheado se confunde con lo nuevo.
    session: String,
    init: Option<Bytes>,
    /// Pista de vídeo y su escala de tiempos, para medir las partes.
    track: u32,
    timescale: u32,
    segs: VecDeque<Seg>,
    next_msn: u64,
    /// Duraciones de las primeras partes normales, para fijar `PART-TARGET`.
    samples: Vec<f64>,
    /// `PART-TARGET` del directo: la parte normal más larga de las medidas al
    /// empezar. No cambia hasta la señal siguiente, porque Safari rechaza la
    /// playlist si cambia o si no encaja con las partes (`err=4`, «formato no
    /// soportado»). ffmpeg entrega fragmentos algo más largos que los 0,25 s
    /// pedidos (los corta en el siguiente fotograma), así que hay que medirlo.
    part_target: Option<f64>,
    /// `TARGETDURATION` anunciado. Solo sube, y solo si un segmento no cabe.
    target: f64,
    /// Playlist ya renderizada; se descarta en cada cambio.
    rendered: Option<String>,
}

impl Inner {
    fn live_msn(&self) -> Option<u64> {
        self.segs.back().filter(|s| s.data.is_none()).map(|s| s.msn)
    }

    fn has_part(&self, msn: u64, idx: Option<usize>) -> bool {
        match self.segs.iter().find(|s| s.msn == msn) {
            // Sin índice, basta con que el segmento exista.
            Some(s) => idx.is_none_or(|i| i < s.parts.len()),
            None => false,
        }
    }

    /// Como `has_part`, pero solo si la playlist ya lo anuncia: las partes de
    /// un segmento sin partes no aparecen hasta que se cierra entero.
    fn announces(&self, msn: u64, idx: Option<usize>) -> bool {
        self.segs.iter().find(|s| s.msn == msn).is_some_and(|s| !s.no_parts || s.data.is_some()) && self.has_part(msn, idx)
    }

    /// Hay playlist que servir: `PART-TARGET` ya está medido y hay algo que
    /// anunciar. Antes, el reproductor espera como en la recarga bloqueante.
    fn ready(&self) -> bool {
        self.part_target.is_some() && self.segs.iter().any(|s| s.data.is_some() || (!s.no_parts && !s.parts.is_empty()))
    }

    fn part(&self, msn: u64, idx: usize) -> Option<Bytes> {
        let s = self.segs.iter().find(|s| s.msn == msn)?;
        s.parts.get(idx).map(|p| p.data.clone())
    }

    fn segment(&self, msn: u64) -> Option<Bytes> {
        self.segs.iter().find(|s| s.msn == msn)?.data.clone()
    }

    /// Duración de un fragmento, medida en el propio MP4. Si no se puede leer
    /// se usa la nominal: una parte con duración muy equivocada descoloca la
    /// cuenta de latencia del reproductor.
    fn frag_dur(&self, moof: &[u8]) -> f64 {
        if self.timescale == 0 {
            return PART_TARGET;
        }
        match moof_duration(moof, self.track) {
            Some(d) if d > 0 => d as f64 / self.timescale as f64,
            _ => PART_TARGET,
        }
    }

    fn push_part(&mut self, data: Bytes, independent: bool) {
        let dur = self.frag_dur(&data);
        self.add_part(data, dur, independent);
    }

    fn add_part(&mut self, data: Bytes, dur: f64, independent: bool) {
        // Un segmento tiene que empezar en keyframe, así que solo se corta ahí.
        let close = independent && self.segs.back().is_some_and(|s| s.dur >= SEG_TARGET - PART_TARGET / 2.0);
        if close {
            self.close_live();
        }
        if self.segs.back().is_none_or(|s| s.data.is_some()) {
            // Sin segmento abierto: uno nuevo, y solo si puede empezar en keyframe.
            if !independent {
                return;
            }
            let msn = self.next_msn;
            self.next_msn += 1;
            self.segs.push_back(Seg { msn, parts: Vec::new(), dur: 0.0, no_parts: false, data: None });
        }
        // Una parte que no cabe en PART-TARGET deja su segmento sin partes.
        let limit = self.part_target.unwrap_or(PART_ODD);
        if self.part_target.is_none() && dur <= limit {
            self.samples.push(dur);
            if self.samples.len() >= PART_SAMPLES {
                let max = self.samples.iter().copied().fold(0.0, f64::max);
                self.part_target = Some((max * 1000.0).ceil() / 1000.0);
            }
        }
        let seg = self.segs.back_mut().expect("hay segmento abierto");
        seg.dur += dur;
        seg.no_parts |= dur > limit;
        seg.parts.push(Part { data, dur, independent });
        while self.segs.len() > KEEP_SEGMENTS {
            self.segs.pop_front();
        }
        self.rendered = None;
    }

    /// Cierra el segmento abierto: a partir de aquí se descarga de una pieza.
    fn close_live(&mut self) {
        let Some(seg) = self.segs.back_mut().filter(|s| s.data.is_none()) else { return };
        let mut buf = BytesMut::with_capacity(seg.parts.iter().map(|p| p.data.len()).sum());
        for p in &seg.parts {
            buf.extend_from_slice(&p.data);
        }
        seg.data = Some(buf.freeze());
        self.rendered = None;
    }

    fn playlist(&mut self) -> String {
        if let Some(p) = &self.rendered {
            return p.clone();
        }
        let seg_max = self.segs.iter().map(|s| s.dur).fold(SEG_TARGET, f64::max).ceil();
        self.target = self.target.max(TARGET_MIN).max(seg_max);
        let target = self.target;
        let first = self.segs.front().map(|s| s.msn).unwrap_or(0);
        let part_target = self.part_target.unwrap_or(PART_TARGET);
        let mut m = String::with_capacity(1024);
        m.push_str("#EXTM3U\n#EXT-X-VERSION:9\n");
        m.push_str(&format!("#EXT-X-TARGETDURATION:{target:.0}\n"));
        // CAN-BLOCK-RELOAD: el reproductor pide la parte siguiente y el origen
        // le deja la petición abierta hasta que existe. Es lo que evita tener
        // que preguntar «¿ya está?» cada poco y lo que baja la latencia.
        m.push_str(&format!(
            "#EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK={:.3},HOLD-BACK={:.3}\n",
            part_target * 4.0,
            target * 3.0
        ));
        m.push_str(&format!("#EXT-X-PART-INF:PART-TARGET={part_target:.3}\n"));
        m.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first}\n"));
        m.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");

        let parts_from = self.segs.len().saturating_sub(PART_SEGMENTS);
        for (i, s) in self.segs.iter().enumerate() {
            if i >= parts_from && !s.no_parts {
                for (j, p) in s.parts.iter().enumerate() {
                    m.push_str(&format!("#EXT-X-PART:DURATION={:.5},URI=\"part/{}/{}.m4s\"", p.dur, s.msn, j));
                    m.push_str(if p.independent { ",INDEPENDENT=YES\n" } else { "\n" });
                }
            }
            if s.data.is_some() {
                m.push_str(&format!("#EXTINF:{:.5},\nseg/{}.m4s\n", s.dur, s.msn));
            }
        }
        // La parte que todavía se está grabando: el reproductor la pide ya y el
        // origen le responde en cuanto ffmpeg la entrega. Si el segmento abierto
        // va sin partes, lo siguiente que se anunciará es el propio segmento.
        if let Some(s) = self.segs.back().filter(|s| !(s.no_parts && s.data.is_none())) {
            let next = if s.data.is_some() { (s.msn + 1, 0) } else { (s.msn, s.parts.len()) };
            m.push_str(&format!("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/{}/{}.m4s\"\n", next.0, next.1));
        }
        self.rendered = Some(m.clone());
        m
    }
}

pub(crate) struct Hls {
    st: Mutex<Inner>,
    /// Sube en cada cambio, para despertar a las peticiones que esperan.
    bump: watch::Sender<u64>,
    /// Clave compartida con el servicio de cuentas. Sin ella, el vídeo es público.
    secret: Option<Vec<u8>>,
}

impl Hls {
    pub fn new(secret: Option<Vec<u8>>) -> Arc<Self> {
        Arc::new(Hls { st: Mutex::new(Inner::default()), bump: watch::channel(0).0, secret })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake(&self) {
        self.bump.send_modify(|v| *v += 1);
    }

    /// Entra un mensaje de vídeo del streamer (byte de tipo + MP4). Devuelve la
    /// ruta de la playlist si acaba de empezar una señal nueva.
    pub fn push(&self, frame: &Bytes) -> Option<String> {
        let (&kind, data) = frame.split_first()?;
        let data = Bytes::copy_from_slice(data);
        let mut st = self.lock();
        let started = if kind == INIT {
            // Señal nueva: sesión nueva, y con ella URLs que nadie tiene cacheadas.
            let (track, timescale) = video_track(&data).unwrap_or((1, 0));
            *st = Inner {
                session: crate::random_token(16),
                init: Some(data),
                track,
                timescale,
                ..Default::default()
            };
            Some(format!("/hls/{}/live.m3u8", st.session))
        } else {
            if st.init.is_none() {
                return None;
            }
            st.push_part(data, kind == FRAG_KEY);
            None
        };
        drop(st);
        self.wake();
        started
    }

    /// El directo ha terminado: lo cacheado deja de servirse.
    pub fn end(&self) {
        *self.lock() = Inner::default();
        self.wake();
    }

    /// Espera a que `ready` se cumpla, hasta `BLOCK_MAX`.
    async fn wait(&self, ready: impl Fn(&Inner) -> bool) {
        let mut rx = self.bump.subscribe();
        let deadline = tokio::time::Instant::now() + BLOCK_MAX;
        loop {
            if ready(&self.lock()) {
                return;
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                return;
            }
        }
    }
}

// ---------- Permiso de acceso ----------

#[derive(Deserialize)]
struct Payload {
    /// Caducidad (segundos desde epoch).
    e: u64,
}

/// Comprueba el token que firma el servicio de cuentas. El ticket de un solo
/// uso del WebSocket no sirve aquí: el reproductor pide decenas de URLs por
/// minuto, así que lleva un token con caducidad que se valida sin consultar a
/// nadie. Solo la playlist lo necesita; de ahí sale la cookie que autoriza el
/// resto de peticiones, así que las URLs de los trozos no llevan nada y el CDN
/// las puede cachear tal cual.
#[doc(hidden)]
pub fn token_ok(secret: &[u8], token: Option<&str>) -> bool {
    let Some(token) = token else { return false };
    let Some((payload, sig)) = token.split_once('.') else { return false };
    let (Ok(sig), Ok(raw)) = (B64.decode(sig), B64.decode(payload)) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else { return false };
    mac.update(payload.as_bytes());
    if mac.verify_slice(&sig).is_err() {
        return false;
    }
    serde_json::from_slice::<Payload>(&raw).is_ok_and(|p| p.e > crate::now_ms() / 1000)
}

/// Caducidad de un token ya validado, para que la cookie dure lo mismo que él.
fn token_exp(token: &str) -> Option<u64> {
    let (payload, _) = token.split_once('.')?;
    let raw = B64.decode(payload).ok()?;
    serde_json::from_slice::<Payload>(&raw).ok().map(|p| p.e)
}

/// Firma un token de acceso. La usa el servicio de cuentas; vive aquí para que
/// las dos partes no se puedan desincronizar.
pub fn sign_token(secret: &[u8], exp: u64) -> Option<String> {
    let payload = B64.encode(serde_json::json!({ "e": exp }).to_string());
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    Some(format!("{payload}.{}", B64.encode(mac.finalize().into_bytes())))
}

// ---------- Servidor HTTP ----------

#[derive(Deserialize)]
struct Q {
    t: Option<String>,
    #[serde(rename = "_HLS_msn")]
    msn: Option<u64>,
    #[serde(rename = "_HLS_part")]
    part: Option<usize>,
}

/// Inmutable: el contenido de estas URLs no cambia nunca, así que el CDN y el
/// navegador se lo pueden quedar y el origen deja de verlas.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
/// Nombre de la cookie que autoriza las peticiones de media.
const COOKIE: &str = "hls";

/// Cookie atada a la sesión: el navegador la manda al pedir el init, los
/// segmentos y las partes, y a nada más del sitio. Se renueva con cada
/// playlist, que es lo que pide el reproductor cuando el token va a caducar.
fn session_cookie(token: &str, session: &str) -> Option<HeaderValue> {
    let left = token_exp(token)?.saturating_sub(crate::now_ms() / 1000);
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/hls/{session}/; HttpOnly; Secure; SameSite=Lax; Max-Age={left}"
    ))
    .ok()
}

fn media(body: Bytes) -> Response {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4")), (header::CACHE_CONTROL, HeaderValue::from_static(IMMUTABLE))],
        Body::from(body),
    )
        .into_response()
}

fn deny() -> Response {
    (StatusCode::FORBIDDEN, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

fn gone() -> Response {
    (StatusCode::NOT_FOUND, [(header::CACHE_CONTROL, "no-store")]).into_response()
}

/// Separa `42.m4s` en 42, y rechaza cualquier otra cosa.
fn msn_of(name: &str) -> Option<u64> {
    name.strip_suffix(".m4s")?.parse().ok()
}

/// Token que autoriza la petición: el de la playlist (`?t=`, el que entrega
/// `/api/ticket`) o el de la cookie que el propio origen dejó al servirla.
fn request_token(q: &Q, headers: &HeaderMap) -> Option<String> {
    if q.t.is_some() {
        return q.t.clone();
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

fn allowed(hls: &Hls, session: &str, token: Option<&str>) -> bool {
    if hls.lock().session != session {
        return false;
    }
    match &hls.secret {
        Some(s) => token_ok(s, token),
        None => true,
    }
}

async fn playlist(State(hls): State<Arc<Hls>>, Path(session): Path<String>, Query(q): Query<Q>, headers: HeaderMap) -> Response {
    let token = request_token(&q, &headers);
    if !allowed(&hls, &session, token.as_deref()) {
        return if hls.lock().session == session { deny() } else { gone() };
    }
    // Recarga bloqueante: el reproductor dice qué parte espera y la respuesta
    // se retiene hasta que existe, en vez de contestar «todavía no». La
    // primera playlist también espera, a que PART-TARGET esté medido.
    if let Some(msn) = q.msn {
        hls.wait(|st| st.announces(msn, q.part) || st.live_msn().is_some_and(|l| l > msn)).await;
    } else {
        hls.wait(Inner::ready).await;
    }
    let (body, session_now) = {
        let mut st = hls.lock();
        if st.session != session {
            return gone();
        }
        if !st.ready() {
            // Recién empezada la señal: que el reproductor vuelva a pedirla.
            return (StatusCode::SERVICE_UNAVAILABLE, [(header::CACHE_CONTROL, "no-store"), (header::RETRY_AFTER, "1")]).into_response();
        }
        (st.playlist(), st.session.clone())
    };
    if session_now != session {
        return gone();
    }
    let mut res = (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl")),
            // La playlist cambia cada parte: que nadie la guarde.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response();
    // Es la única petición que lleva el token en la URL; de aquí sale la
    // cookie con la que el reproductor pedirá los trozos.
    if let Some(cookie) = token.as_deref().and_then(|t| session_cookie(t, &session)) {
        res.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    res
}

async fn init(State(hls): State<Arc<Hls>>, Path(session): Path<String>, Query(q): Query<Q>, headers: HeaderMap) -> Response {
    let token = request_token(&q, &headers);
    if !allowed(&hls, &session, token.as_deref()) {
        return deny();
    }
    match hls.lock().init.clone() {
        Some(b) => media(b),
        None => gone(),
    }
}

async fn segment(State(hls): State<Arc<Hls>>, Path((session, name)): Path<(String, String)>, Query(q): Query<Q>, headers: HeaderMap) -> Response {
    let token = request_token(&q, &headers);
    if !allowed(&hls, &session, token.as_deref()) {
        return deny();
    }
    let Some(msn) = msn_of(&name) else { return gone() };
    match hls.lock().segment(msn) {
        Some(b) => media(b),
        None => gone(),
    }
}

async fn part(State(hls): State<Arc<Hls>>, Path((session, msn, name)): Path<(String, u64, String)>, Query(q): Query<Q>, headers: HeaderMap) -> Response {
    let token = request_token(&q, &headers);
    if !allowed(&hls, &session, token.as_deref()) {
        return deny();
    }
    let Some(idx) = name.strip_suffix(".m4s").and_then(|n| n.parse::<usize>().ok()) else { return gone() };
    // El reproductor pide por adelantado la parte que aún se está grabando
    // (EXT-X-PRELOAD-HINT): se le responde en cuanto ffmpeg la entrega.
    hls.wait(|st| st.has_part(msn, Some(idx)) || st.live_msn().is_some_and(|l| l > msn)).await;
    match hls.lock().part(msn, idx) {
        Some(b) => media(b),
        None => gone(),
    }
}

pub(crate) async fn serve(listener: TcpListener, hls: Arc<Hls>) {
    let app = router(hls);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Origen HLS: {e}");
    }
}

fn router(hls: Arc<Hls>) -> Router {
    Router::new()
        .route("/hls/:session/live.m3u8", get(playlist))
        .route("/hls/:session/init.mp4", get(init))
        .route("/hls/:session/seg/:name", get(segment))
        .route("/hls/:session/part/:msn/:name", get(part))
        .with_state(hls)
}

// ---------- Acceso para las pruebas ----------

/// Envuelve el empaquetador sin el servidor HTTP, para poder comprobar el
/// empaquetado contra vídeo real de ffmpeg.
#[doc(hidden)]
pub struct Packager(Arc<Hls>);

impl Packager {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Packager(Hls::new(None))
    }

    pub fn with_secret(secret: &[u8]) -> Self {
        Packager(Hls::new(Some(secret.to_vec())))
    }

    /// Sirve este empaquetador por HTTP en un puerto libre y devuelve la base
    /// (`http://127.0.0.1:puerto`), para probarlo como lo ve el reproductor.
    pub async fn serve_local(&self) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("puerto libre");
        let base = format!("http://{}", l.local_addr().expect("dirección"));
        let app = router(self.0.clone());
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        base
    }

    /// Entrega un fragmento. `true` si con él empieza una sesión nueva.
    pub fn push_frame(&self, kind: u8, data: &[u8]) -> bool {
        let mut frame = BytesMut::with_capacity(data.len() + 1);
        frame.extend_from_slice(&[kind]);
        frame.extend_from_slice(data);
        self.0.push(&frame.freeze()).is_some()
    }

    /// Entrega un fragmento como si durase `dur` segundos, para simular la
    /// parte larga con la que arranca el encoder.
    pub fn push_frame_dur(&self, kind: u8, data: &[u8], dur: f64) {
        let mut st = self.0.lock();
        if st.init.is_some() {
            st.add_part(Bytes::copy_from_slice(data), dur, kind == FRAG_KEY);
        }
        drop(st);
        self.0.wake();
    }

    pub fn end(&self) {
        self.0.end();
    }

    pub fn playlist(&self) -> String {
        self.0.lock().playlist()
    }

    pub fn part(&self, msn: u64, idx: usize) -> Option<Bytes> {
        self.0.lock().part(msn, idx)
    }

    pub fn segment(&self, msn: u64) -> Option<Bytes> {
        self.0.lock().segment(msn)
    }

    pub fn segment_count(&self) -> usize {
        self.0.lock().segs.len()
    }

    pub fn session(&self) -> String {
        self.0.lock().session.clone()
    }
}

// ---------- Lectura del MP4 ----------

/// Pista de vídeo del `moov` y su escala de tiempos.
fn video_track(init: &[u8]) -> Option<(u32, u32)> {
    // El init es ftyp + moov, uno detrás del otro.
    let (_, moov) = children(init).find(|(k, _)| *k == b"moov")?;
    for (kind, trak) in children(moov) {
        if kind != b"trak" {
            continue;
        }
        let (mut id, mut timescale, mut is_video) = (None, None, false);
        for (kind, b) in children(trak) {
            match kind {
                b"tkhd" => id = be32(b, if b.first() == Some(&1) { 20 } else { 12 }),
                b"mdia" => {
                    for (kind, m) in children(b) {
                        match kind {
                            b"mdhd" => timescale = be32(m, if m.first() == Some(&1) { 20 } else { 12 }),
                            b"hdlr" => is_video = m.get(8..12) == Some(&b"vide"[..]),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if is_video {
            return Some((id?, timescale?));
        }
    }
    None
}

/// Suma la duración de las muestras de `track` en un `moof`, en unidades de la
/// escala de tiempos de la pista.
fn moof_duration(moof: &[u8], track: u32) -> Option<u64> {
    for (kind, traf) in children(moof.get(8..)?) {
        if kind != b"traf" {
            continue;
        }
        let (mut mine, mut default_dur, mut total, mut seen) = (false, 0u32, 0u64, false);
        for (kind, b) in children(traf) {
            let Some(fl) = be32(b, 0).map(|v| v & 0x00ff_ffff) else { continue };
            match kind {
                b"tfhd" => {
                    if be32(b, 4) != Some(track) {
                        break;
                    }
                    mine = true;
                    let mut i = 8;
                    for (bit, len) in [(0x1, 8), (0x2, 4)] {
                        if fl & bit != 0 {
                            i += len;
                        }
                    }
                    if fl & 0x8 != 0 {
                        default_dur = be32(b, i)?;
                    }
                }
                b"trun" if mine => {
                    let count = be32(b, 4)?;
                    let mut i = 8 + if fl & 0x1 != 0 { 4 } else { 0 } + if fl & 0x4 != 0 { 4 } else { 0 };
                    let stride = [0x100, 0x200, 0x400, 0x800].iter().filter(|b| fl & *b != 0).count() * 4;
                    for _ in 0..count {
                        total += if fl & 0x100 != 0 { be32(b, i)? as u64 } else { default_dur as u64 };
                        i += stride;
                    }
                    seen = true;
                }
                _ => {}
            }
        }
        if mine && seen {
            return Some(total);
        }
    }
    None
}
