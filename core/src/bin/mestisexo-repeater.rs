//! Repetidor para el VPS: `mestisexo-repeater [--listen 127.0.0.1:8090] [--hls-listen 127.0.0.1:8094]
//! [--upstream 127.0.0.1:8091] [--max-viewers 500] [--auth-internal 127.0.0.1:8093 --auth-url https://…]
//! [--events-listen 127.0.0.1:8095 [--events-grace 60]]`
//!
//! El vídeo sale por `--hls-listen` como LL-HLS, que Caddy (y cualquier CDN
//! delante) puede cachear. Por `--listen` solo va el chat, por WebSocket.
use std::net::SocketAddr;

fn main() {
    let mut cfg = mestisexo_core::RepeaterConfig {
        listen: "127.0.0.1:8090".parse().unwrap(),
        hls_listen: "127.0.0.1:8094".parse().unwrap(),
        upstream: SocketAddr::from(([127, 0, 0, 1], mestisexo_core::REPEATER_UPSTREAM_PORT)),
        max_viewers: 500,
        auth: None,
        hls_secret: None,
        events: None,
    };
    let (mut auth_internal, mut auth_url) = (None, None);
    let mut events_listen: Option<SocketAddr> = None;
    let mut grace = std::time::Duration::from_secs(60);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v = args.next().unwrap_or_else(|| usage(&format!("falta el valor de {a}")));
        match a.as_str() {
            "--listen" => cfg.listen = v.parse().unwrap_or_else(|_| usage("--listen inválido")),
            "--hls-listen" => cfg.hls_listen = v.parse().unwrap_or_else(|_| usage("--hls-listen inválido")),
            "--upstream" => cfg.upstream = v.parse().unwrap_or_else(|_| usage("--upstream inválido")),
            "--max-viewers" => cfg.max_viewers = v.parse().unwrap_or_else(|_| usage("--max-viewers inválido")),
            "--auth-internal" => auth_internal = Some(v.parse().unwrap_or_else(|_| usage("--auth-internal inválido"))),
            "--auth-url" if v.starts_with("https://") => auth_url = Some(v.trim_end_matches('/').to_string()),
            "--events-listen" => events_listen = Some(v.parse().unwrap_or_else(|_| usage("--events-listen inválido"))),
            "--events-grace" => grace = std::time::Duration::from_secs(v.parse().unwrap_or_else(|_| usage("--events-grace inválido (segundos)"))),
            _ => usage(&format!("opción desconocida: {a}")),
        }
    }
    cfg.auth = match (auth_internal, auth_url) {
        (Some(internal), Some(public_url)) => Some(mestisexo_core::RepeaterAuth { internal, public_url }),
        (None, None) => None,
        _ => usage("--auth-internal y --auth-url van juntos (la URL debe ser https://)"),
    };
    // La clave que firma el acceso al vídeo llega por el entorno, no por la
    // línea de órdenes: ahí la vería cualquiera con `ps`. Es la misma que lee
    // el servicio de cuentas de /etc/mestisexo/auth.env.
    cfg.hls_secret = std::env::var("HLS_SECRET").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).map(|s| {
        if s.len() < 32 {
            usage("HLS_SECRET debe tener al menos 32 caracteres");
        }
        s.into_bytes()
    });
    if cfg.hls_secret.is_none() && cfg.auth.is_some() {
        eprintln!("Aviso: sin HLS_SECRET el chat pide cuenta de Discord pero el vídeo queda accesible a quien tenga la URL.");
    }
    // Igual que HLS_SECRET: el token del bot llega por el entorno. Si falta, se
    // avisa y se sigue sin avisos: el directo no depende de ellos.
    if let Some(listen) = events_listen {
        let token = std::env::var("EVENTS_TOKEN").ok().map(|s| s.trim().to_string()).unwrap_or_default();
        if token.len() < 32 {
            eprintln!("Aviso: --events-listen sin EVENTS_TOKEN de al menos 32 caracteres; los avisos del directo quedan desactivados.");
        } else {
            cfg.events = Some(mestisexo_core::EventsConfig { listen, token, grace });
        }
    }
    // Multihilo: cada espectador es una tarea y el reparto se distribuye entre
    // los núcleos del VPS. Con un solo hilo el techo era ~700 espectadores por
    // mucho que el VPS tuviera 4 núcleos, porque tres quedaban sin usar.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    if let Err(e) = rt.block_on(mestisexo_core::run_repeater(cfg)) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn usage(msg: &str) -> ! {
    eprintln!("{msg}\nUso: mestisexo-repeater [--listen IP:PUERTO] [--hls-listen IP:PUERTO] [--upstream IP:PUERTO] [--max-viewers N] [--auth-internal IP:PUERTO --auth-url https://…] [--events-listen IP:PUERTO [--events-grace SEGUNDOS]]\nEntorno: HLS_SECRET firma el acceso al vídeo (la misma clave que el servicio de cuentas). EVENTS_TOKEN autoriza al bot en /events.");
    std::process::exit(2);
}
