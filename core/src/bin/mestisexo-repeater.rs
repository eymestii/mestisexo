//! Repetidor para el VPS: `mestisexo-repeater [--listen 127.0.0.1:8090] [--upstream 127.0.0.1:8091] [--max-viewers 500]
//! [--auth-internal 127.0.0.1:8093 --auth-url https://…]`
use std::net::SocketAddr;

fn main() {
    let mut cfg = mestisexo_core::RepeaterConfig {
        listen: "127.0.0.1:8090".parse().unwrap(),
        upstream: SocketAddr::from(([127, 0, 0, 1], mestisexo_core::REPEATER_UPSTREAM_PORT)),
        max_viewers: 500,
        auth: None,
    };
    let (mut auth_internal, mut auth_url) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v = args.next().unwrap_or_else(|| usage(&format!("falta el valor de {a}")));
        match a.as_str() {
            "--listen" => cfg.listen = v.parse().unwrap_or_else(|_| usage("--listen inválido")),
            "--upstream" => cfg.upstream = v.parse().unwrap_or_else(|_| usage("--upstream inválido")),
            "--max-viewers" => cfg.max_viewers = v.parse().unwrap_or_else(|_| usage("--max-viewers inválido")),
            "--auth-internal" => auth_internal = Some(v.parse().unwrap_or_else(|_| usage("--auth-internal inválido"))),
            "--auth-url" if v.starts_with("https://") => auth_url = Some(v.trim_end_matches('/').to_string()),
            _ => usage(&format!("opción desconocida: {a}")),
        }
    }
    cfg.auth = match (auth_internal, auth_url) {
        (Some(internal), Some(public_url)) => Some(mestisexo_core::RepeaterAuth { internal, public_url }),
        (None, None) => None,
        _ => usage("--auth-internal y --auth-url van juntos (la URL debe ser https://)"),
    };
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
    eprintln!("{msg}\nUso: mestisexo-repeater [--listen IP:PUERTO] [--upstream IP:PUERTO] [--max-viewers N] [--auth-internal IP:PUERTO --auth-url https://…]");
    std::process::exit(2);
}
