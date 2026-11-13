//! Avisos de inicio y fin del directo por Server-Sent Events, para un bot que
//! los publique en Discord.
//!
//! `GET /events` con `Authorization: Bearer <EVENTS_TOKEN>` deja la respuesta
//! abierta: nada más conectar llega el estado actual, y después un evento en
//! cada cambio. No hay que preguntar cada poco; entre evento y evento solo
//! viaja un comentario de keep-alive cada 15 s.
//!
//! Un corte del túnel o reiniciar OBS no es el fin del directo: el «offline»
//! solo se publica si en `grace` no ha vuelto la señal. Si vuelve, sigue
//! siendo el mismo directo, con el mismo `since`. Al pulsar «Salir» en la app
//! (evento «end») sí termina al momento.

use crate::now_ms;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Conexiones a la vez. Basta con una (el bot); el resto es margen para
/// reconexiones que se solapan.
const MAX_CLIENTS: usize = 4;

pub struct EventsConfig {
    /// Dirección donde escucha, p. ej. `127.0.0.1:8095`. Solo loopback: el bot
    /// corre en el mismo VPS.
    pub listen: SocketAddr,
    /// Lo que el bot manda como `Authorization: Bearer …`.
    pub token: String,
    /// Tiempo sin señal antes de dar el directo por terminado.
    pub grace: Duration,
}

/// Lo que ve el bot. `since` identifica el directo: no cambia por un corte
/// corto, así que el bot no lo anuncia dos veces.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Published {
    live: bool,
    /// Inicio del directo actual o del último (ms desde epoch). 0 si aún no hubo ninguno.
    since: u64,
    /// Fin del último directo (ms desde epoch), 0 mientras sigue.
    ended: u64,
}

/// Lo que llega del streamer, antes de aplicar el margen.
struct Raw {
    live: bool,
    /// Sube en cada cambio, para que un temporizador viejo no publique nada.
    gen: u64,
}

pub(crate) struct Events {
    tx: watch::Sender<Published>,
    raw: Mutex<Raw>,
    grace: Duration,
    token: String,
    clients: AtomicUsize,
}

impl Events {
    pub fn new(token: String, grace: Duration) -> Arc<Self> {
        Arc::new(Events {
            tx: watch::channel(Published::default()).0,
            raw: Mutex::new(Raw { live: false, gen: 0 }),
            grace,
            token,
            clients: AtomicUsize::new(0),
        })
    }

    fn raw(&self) -> MutexGuard<'_, Raw> {
        self.raw.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Ha empezado una señal. Si el directo seguía abierto (corte corto), no cambia nada.
    pub fn live(&self) {
        let mut r = self.raw();
        r.live = true;
        r.gen += 1;
        self.tx.send_if_modified(|p| {
            if p.live {
                return false;
            }
            // Siempre mayor que el anterior, aunque el reloj no haya avanzado.
            *p = Published { live: true, since: now_ms().max(p.since + 1), ended: 0 };
            true
        });
    }

    /// Se ha ido la señal. Con `now`, el directo termina ya; si no, tras el margen.
    pub fn offline(self: &Arc<Self>, now: bool) {
        let mut r = self.raw();
        r.live = false;
        r.gen += 1;
        if now || self.grace.is_zero() {
            self.publish_offline();
            return;
        }
        let gen = r.gen;
        drop(r);
        let me = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(me.grace).await;
            let r = me.raw();
            if r.gen == gen && !r.live {
                me.publish_offline();
            }
        });
    }

    /// Se llama con `raw` tomado, para no cruzarse con un `live` a la vez.
    fn publish_offline(&self) {
        self.tx.send_if_modified(|p| {
            if !p.live {
                return false;
            }
            p.live = false;
            p.ended = now_ms();
            true
        });
    }
}

/// Plaza de conexión: se devuelve al cerrarse la respuesta.
struct Slot(Arc<Events>);

impl Slot {
    fn take(ev: &Arc<Events>) -> Option<Slot> {
        let n = ev.clients.fetch_add(1, Ordering::AcqRel);
        if n >= MAX_CLIENTS {
            ev.clients.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Slot(ev.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Comparación en tiempo constante: no deja adivinar el token carácter a carácter.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(token: &str, headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| same(t.as_bytes(), token.as_bytes()))
}

fn to_event(p: Published) -> Event {
    let data = serde_json::json!({
        "live": p.live,
        "since": p.since,
        "ended": p.ended,
        "duration_s": if p.ended > p.since { (p.ended - p.since) / 1000 } else { 0 },
    });
    Event::default().event(if p.live { "live" } else { "offline" }).id(p.since.to_string()).data(data.to_string())
}

async fn stream(State(ev): State<Arc<Events>>, headers: HeaderMap) -> Response {
    if !authorized(&ev.token, &headers) {
        return (StatusCode::UNAUTHORIZED, [(header::CACHE_CONTROL, "no-store")]).into_response();
    }
    let Some(slot) = Slot::take(&ev) else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::CACHE_CONTROL, "no-store")]).into_response();
    };
    let rx = ev.tx.subscribe();
    // Primero el estado actual, luego cada cambio. `slot` vive lo que la conexión.
    let events = futures_util::stream::unfold((rx, true, slot), |(mut rx, first, slot)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let p = *rx.borrow_and_update();
        Some((Ok::<_, Infallible>(to_event(p)), (rx, false, slot)))
    });
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

pub(crate) async fn serve(listener: TcpListener, ev: Arc<Events>) {
    let app = Router::new().route("/events", get(stream)).with_state(ev);
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Eventos: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn un_corte_corto_no_termina_el_directo() {
        let ev = Events::new("x".into(), Duration::from_secs(60));
        let rx = ev.tx.subscribe();
        assert!(!rx.borrow().live);

        ev.live();
        let since = rx.borrow().since;
        assert!(rx.borrow().live && since > 0);

        // Se corta y vuelve antes del margen: sigue siendo el mismo directo.
        ev.offline(false);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(rx.borrow().live);
        ev.live();
        tokio::time::sleep(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;
        assert_eq!(*rx.borrow(), Published { live: true, since, ended: 0 });

        // Se va de verdad: offline pasado el margen, con el mismo `since`.
        ev.offline(false);
        tokio::time::sleep(Duration::from_secs(61)).await;
        tokio::task::yield_now().await;
        let p = *rx.borrow();
        assert!(!p.live && p.since == since && p.ended > 0);

        // Un directo nuevo tiene otro `since`, y «end» lo cierra al momento.
        ev.live();
        assert!(rx.borrow().live && rx.borrow().since > since);
        ev.offline(true);
        assert!(!rx.borrow().live);
    }

    #[test]
    fn token() {
        let mut h = HeaderMap::new();
        assert!(!authorized("secreto", &h));
        h.insert(header::AUTHORIZATION, "Bearer otro".parse().unwrap());
        assert!(!authorized("secreto", &h));
        h.insert(header::AUTHORIZATION, "Bearer secreto".parse().unwrap());
        assert!(authorized("secreto", &h));
    }
}
