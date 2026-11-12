//! Repetidor para el VPS: `mestisexo-repeater [--listen 0.0.0.0:8090] [--upstream 127.0.0.1:8091] [--max-viewers 500]`
use std::net::SocketAddr;

fn main() {
    let mut cfg = mestisexo_core::RepeaterConfig {
        listen: "0.0.0.0:8090".parse().unwrap(),
        upstream: SocketAddr::from(([127, 0, 0, 1], mestisexo_core::REPEATER_UPSTREAM_PORT)),
        max_viewers: 500,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let v = args.next().unwrap_or_else(|| usage(&format!("falta el valor de {a}")));
        match a.as_str() {
            "--listen" => cfg.listen = v.parse().unwrap_or_else(|_| usage("--listen inválido")),
            "--upstream" => cfg.upstream = v.parse().unwrap_or_else(|_| usage("--upstream inválido")),
            "--max-viewers" => cfg.max_viewers = v.parse().unwrap_or_else(|_| usage("--max-viewers inválido")),
            _ => usage(&format!("opción desconocida: {a}")),
        }
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    if let Err(e) = rt.block_on(mestisexo_core::run_repeater(cfg)) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

fn usage(msg: &str) -> ! {
    eprintln!("{msg}\nUso: mestisexo-repeater [--listen IP:PUERTO] [--upstream IP:PUERTO] [--max-viewers N]");
    std::process::exit(2);
}
