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

use crate::video::{be32, children};
use crate::{FRAG_KEY, INIT};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
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
/// Un segmento se cierra en el primer keyframe a partir de esta duración.
const SEG_TARGET: f64 = 2.0;
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
    /// Parte más larga vista. Se anuncia esto como `PART-TARGET` porque la
    /// especificación no permite que ninguna parte lo supere, y ffmpeg entrega
    /// fragmentos algo más largos que los 0,25 s pedidos (los corta en el
    /// siguiente fotograma, no en el instante exacto).
    max_part: f64,
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
            self.segs.push_back(Seg { msn, parts: Vec::new(), dur: 0.0, data: None });
        }
        let seg = self.segs.back_mut().expect("hay segmento abierto");
        seg.dur += dur;
        seg.parts.push(Part { data, dur, independent });
        self.max_part = self.max_part.max(dur);
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
        let target = self.segs.iter().map(|s| s.dur).fold(SEG_TARGET, f64::max).ceil().max(1.0);
        let first = self.segs.front().map(|s| s.msn).unwrap_or(0);
        // Ninguna parte puede durar más que PART-TARGET, así que se anuncia lo
        // medido y no los 0,25 s nominales.
        let part_target = (self.max_part.max(PART_TARGET) * 1000.0).ceil() / 1000.0;
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
            if i >= parts_from {
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
        // origen le responde en cuanto ffmpeg la entrega.
        if let Some(s) = self.segs.back() {
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
/// nadie. Como el contenido no depende de quién lo pida, el CDN debe dejar
/// este parámetro fuera de la clave de caché.
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

fn allowed(hls: &Hls, session: &str, q: &Q) -> bool {
    if hls.lock().session != session {
        return false;
    }
    match &hls.secret {
        Some(s) => token_ok(s, q.t.as_deref()),
        None => true,
    }
}

async fn playlist(State(hls): State<Arc<Hls>>, Path(session): Path<String>, Query(q): Query<Q>) -> Response {
    if !allowed(&hls, &session, &q) {
        return if hls.lock().session == session { deny() } else { gone() };
    }
    // Recarga bloqueante: el reproductor dice qué parte espera y la respuesta
    // se retiene hasta que existe, en vez de contestar «todavía no».
    if let Some(msn) = q.msn {
        hls.wait(|st| st.has_part(msn, q.part) || st.live_msn().is_some_and(|l| l > msn)).await;
    }
    let (body, session_now) = {
        let mut st = hls.lock();
        (st.playlist(), st.session.clone())
    };
    if session_now != session {
        return gone();
    }
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.apple.mpegurl")),
            // La playlist cambia cada parte: que nadie la guarde.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

async fn init(State(hls): State<Arc<Hls>>, Path(session): Path<String>, Query(q): Query<Q>) -> Response {
    if !allowed(&hls, &session, &q) {
        return deny();
    }
    match hls.lock().init.clone() {
        Some(b) => media(b),
        None => gone(),
    }
}

async fn segment(State(hls): State<Arc<Hls>>, Path((session, name)): Path<(String, String)>, Query(q): Query<Q>) -> Response {
    if !allowed(&hls, &session, &q) {
        return deny();
    }
    let Some(msn) = msn_of(&name) else { return gone() };
    match hls.lock().segment(msn) {
        Some(b) => media(b),
        None => gone(),
    }
}

async fn part(State(hls): State<Arc<Hls>>, Path((session, msn, name)): Path<(String, u64, String)>, Query(q): Query<Q>) -> Response {
    if !allowed(&hls, &session, &q) {
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
