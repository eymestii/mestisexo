#[tokio::main]
async fn main() {
    let h = mestisexo_core::start(mestisexo_core::Config {
        ws_port: 18095, rtmp_port: 11936, ffmpeg: "ffmpeg".into(),
        tunnel: Some(mestisexo_core::Tunnel { target: "sexoserver".into(), identity: None, upstream_port: 18091 }),
        viewer_key: Some("prueba123".into()), stream_key: Some("sk".into()),
    }).await.unwrap();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let s = h.status();
        println!("viewers={} tunnel={:?} live={}", s.viewers, s.tunnel, s.live);
    }
}
