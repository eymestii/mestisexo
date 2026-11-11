//! Servidor sin interfaz, útil para pruebas: `cargo run -p mestisexo-core --example server`
#[tokio::main]
async fn main() {
    let h = mestisexo_core::start(mestisexo_core::Config {
        ws_port: 8090,
        rtmp_port: 1935,
        ffmpeg: "ffmpeg".into(),
        tunnel: std::env::var("VPS").ok().map(|target| mestisexo_core::Tunnel {
            target,
            identity: std::env::var("VPS_KEY").ok(),
            remote_port: 8090,
        }),
        viewer_key: None,
        stream_key: None,
    })
    .await
    .expect("start");
    println!("RTMP: {} clave: {}", h.rtmp_server, h.stream_key);
    println!("WS: ws://127.0.0.1:{}/?k={}", h.ws_port, h.key());
    println!("HOST: {}", h.host_token());
    for _ in 0..8 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        println!("estado: {:?}", h.status().tunnel);
    }
    tokio::signal::ctrl_c().await.unwrap();
    h.stop().await;
}
