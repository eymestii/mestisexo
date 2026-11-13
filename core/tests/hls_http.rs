//! Comprueba el origen LL-HLS como lo ve un reproductor: por HTTP, con la
//! recarga bloqueante y el permiso de acceso.
//!
//! `cargo test -p mestisexo-core --test hls_http` (necesita ffmpeg en el PATH).

use mestisexo_core::testing::{self as t, Packager};
use std::process::{Command, Stdio};
use std::time::Duration;

fn sample_mp4(seconds: u32) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", &format!("testsrc=size=320x240:rate=30:duration={seconds}")])
        .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}")])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-g", "30"])
        .args(["-c:a", "aac", "-b:a", "64k"])
        .args([
            "-f", "mp4", "-frag_duration", "250000",
            "-movflags", "frag_keyframe+empty_moov+default_base_moof",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .expect("ffmpeg no está disponible");
    assert!(out.status.success(), "ffmpeg falló");
    out.stdout
}

async fn get(url: &str) -> (u16, Vec<u8>, String) {
    let r = reqwest::get(url).await.expect("petición");
    let status = r.status().as_u16();
    let cache = r.headers().get("cache-control").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    (status, r.bytes().await.expect("cuerpo").to_vec(), cache)
}

/// Como `get`, pero con la cookie que el navegador mandaría solo.
async fn get_with_cookie(url: &str, cookie: &str) -> (u16, Vec<u8>) {
    let r = reqwest::Client::new().get(url).header("cookie", cookie).send().await.expect("petición");
    (r.status().as_u16(), r.bytes().await.expect("cuerpo").to_vec())
}

#[tokio::test]
async fn el_reproductor_descarga_todo_lo_anunciado() {
    let frames = t::split_frames(&sample_mp4(6));
    let hls = Packager::new();
    for (k, d) in &frames {
        hls.push_frame(*k, d);
    }
    let base = hls.serve_local().await;
    let session = hls.session();

    let (code, body, cache) = get(&format!("{base}/hls/{session}/live.m3u8")).await;
    assert_eq!(code, 200);
    assert_eq!(cache, "no-store", "la playlist no se puede cachear");
    let m = String::from_utf8(body).expect("playlist en texto");

    let (code, init, cache) = get(&format!("{base}/hls/{session}/init.mp4")).await;
    assert_eq!(code, 200);
    assert!(!init.is_empty());
    assert!(cache.contains("immutable"), "el init debe poder cachearse: {cache}");

    // Todo lo que la playlist anuncia tiene que existir, con la cabecera que
    // permite al CDN quedárselo.
    for uri in uris(&m) {
        let (code, body, cache) = get(&format!("{base}/hls/{session}/{uri}")).await;
        assert_eq!(code, 200, "{uri} devolvió {code}");
        assert!(!body.is_empty(), "{uri} vacío");
        assert!(cache.contains("immutable"), "{uri} sin caché: {cache}");
    }
}

#[tokio::test]
async fn la_recarga_bloqueante_espera_a_la_parte() {
    let frames = t::split_frames(&sample_mp4(6));
    let hls = Packager::new();
    // Solo el init y el primer tramo: la parte que se pedirá no existe aún.
    let primeras = 6;
    for (k, d) in frames.iter().take(primeras) {
        hls.push_frame(*k, d);
    }
    let base = hls.serve_local().await;
    let session = hls.session();

    let m = String::from_utf8(get(&format!("{base}/hls/{session}/live.m3u8")).await.1).unwrap();
    let hint = m
        .lines()
        .find_map(|l| l.strip_prefix("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\""))
        .map(|l| l.trim_end_matches('"'))
        .expect("falta el aviso de la parte siguiente")
        .to_string();

    // La parte avisada todavía no existe: la petición debe quedarse esperando.
    let url = format!("{base}/hls/{session}/{hint}");
    let pendiente = tokio::spawn(async move { get(&url).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!pendiente.is_finished(), "debería estar esperando la parte, no contestando ya");

    // En cuanto llega el vídeo, la respuesta sale.
    for (k, d) in frames.iter().skip(primeras).take(4) {
        hls.push_frame(*k, d);
    }
    let (code, body, _) = tokio::time::timeout(Duration::from_secs(3), pendiente)
        .await
        .expect("la espera no debería agotarse")
        .expect("tarea");
    assert_eq!(code, 200);
    assert!(!body.is_empty());
}

#[tokio::test]
async fn sin_token_no_hay_video() {
    let secret = b"una clave de al menos treinta y dos bytes";
    let frames = t::split_frames(&sample_mp4(4));
    let hls = Packager::with_secret(secret);
    for (k, d) in &frames {
        hls.push_frame(*k, d);
    }
    let base = hls.serve_local().await;
    let session = hls.session();

    assert_eq!(get(&format!("{base}/hls/{session}/live.m3u8")).await.0, 403, "sin token no se entra");
    assert_eq!(get(&format!("{base}/hls/{session}/init.mp4")).await.0, 403);

    let ahora = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let caducado = mestisexo_core::sign_token(secret, ahora - 1).unwrap();
    assert_eq!(get(&format!("{base}/hls/{session}/live.m3u8?t={caducado}")).await.0, 403, "token caducado");

    let bueno = mestisexo_core::sign_token(secret, ahora + 600).unwrap();
    let (code, body, _) = get(&format!("{base}/hls/{session}/live.m3u8?t={bueno}")).await;
    assert_eq!(code, 200, "con token válido sí");
    let m = String::from_utf8(body).unwrap();
    for uri in uris(&m).into_iter().take(4) {
        assert_eq!(get(&format!("{base}/hls/{session}/{uri}?t={bueno}")).await.0, 200, "{uri}");
        assert_eq!(get(&format!("{base}/hls/{session}/{uri}")).await.0, 403, "{uri} sin token");
    }
}

#[tokio::test]
async fn la_playlist_deja_una_cookie_que_abre_los_trozos() {
    let secret = b"una clave de al menos treinta y dos bytes";
    let frames = t::split_frames(&sample_mp4(6));
    let hls = Packager::with_secret(secret);
    for (k, d) in &frames {
        hls.push_frame(*k, d);
    }
    let base = hls.serve_local().await;
    let session = hls.session();

    let ahora = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let bueno = mestisexo_core::sign_token(secret, ahora + 600).unwrap();

    // El reproductor pide la playlist con el token y el origen deja la cookie.
    let r = reqwest::get(format!("{base}/hls/{session}/live.m3u8?t={bueno}")).await.expect("playlist");
    assert_eq!(r.status().as_u16(), 200);
    let set_cookie = r.headers().get("set-cookie").expect("debe dejar cookie").to_str().unwrap().to_string();
    assert!(set_cookie.starts_with(&format!("hls={bueno};")), "{set_cookie}");
    assert!(set_cookie.contains(&format!("Path=/hls/{session}/")), "la cookie no se limita a la sesión: {set_cookie}");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("Secure"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    let m = String::from_utf8(r.bytes().await.unwrap().to_vec()).unwrap();

    // Con la cookie y sin token en la URL, todo se descarga: es lo que hace el
    // navegador al pedir init, segmentos y partes.
    let (code, init) = get_with_cookie(&format!("{base}/hls/{session}/init.mp4"), &cookie).await;
    assert_eq!(code, 200);
    assert!(!init.is_empty());
    for uri in uris(&m).into_iter().take(4) {
        let (code, body) = get_with_cookie(&format!("{base}/hls/{session}/{uri}"), &cookie).await;
        assert_eq!(code, 200, "{uri} con cookie devolvió {code}");
        assert!(!body.is_empty(), "{uri} vacío");
    }

    // Sin cookie y sin token sigue cerrado. El `Path` lo aplica el navegador;
    // el servidor lo que comprueba es la firma del token que traiga la cookie.
    assert_eq!(get(&format!("{base}/hls/{session}/init.mp4")).await.0, 403);
    let falsa = format!("hls={}.AAAA", bueno.split('.').next().unwrap());
    assert_eq!(get_with_cookie(&format!("{base}/hls/{session}/init.mp4"), &falsa).await.0, 403);

    // La cookie caduca con el token, no antes.
    let caducado = mestisexo_core::sign_token(secret, ahora - 1).unwrap();
    let r = reqwest::get(format!("{base}/hls/{session}/live.m3u8?t={caducado}")).await.expect("playlist");
    assert_eq!(r.status().as_u16(), 403);
    assert!(r.headers().get("set-cookie").is_none(), "un token caducado no debe dejar cookie");
}

#[tokio::test]
async fn al_terminar_el_directo_lo_cacheado_deja_de_servirse() {
    let frames = t::split_frames(&sample_mp4(4));
    let hls = Packager::new();
    for (k, d) in &frames {
        hls.push_frame(*k, d);
    }
    let base = hls.serve_local().await;
    let session = hls.session();
    let m = String::from_utf8(get(&format!("{base}/hls/{session}/live.m3u8")).await.1).unwrap();
    let alguna = uris(&m).into_iter().next().expect("hay URIs");
    assert_eq!(get(&format!("{base}/hls/{session}/{alguna}")).await.0, 200);

    hls.end();
    assert_eq!(get(&format!("{base}/hls/{session}/live.m3u8")).await.0, 404, "la sesión anterior ya no existe");
    assert_eq!(get(&format!("{base}/hls/{session}/{alguna}")).await.0, 403);
}

/// URIs de segmentos y partes que la playlist anuncia, sin el aviso de la
/// parte siguiente (que por definición aún no existe).
fn uris(m: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in m.lines() {
        if let Some(rest) = l.strip_prefix("#EXT-X-PART:") {
            if let Some(i) = rest.find("URI=\"") {
                let u = &rest[i + 5..];
                v.push(u[..u.find('"').unwrap_or(u.len())].to_string());
            }
        } else if l.starts_with("seg/") {
            v.push(l.to_string());
        }
    }
    v
}
