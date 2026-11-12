//! Comprueba el empaquetado LL-HLS contra vídeo real generado por ffmpeg, con
//! los mismos parámetros que usa `video.rs`.
//!
//! `cargo test -p mestisexo-core --test hls` (necesita ffmpeg en el PATH).

use std::process::{Command, Stdio};

/// Genera unos segundos de H.264 + AAC en fMP4 fragmentado, igual que `video.rs`.
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
    assert!(!out.stdout.is_empty(), "ffmpeg no produjo nada");
    out.stdout
}

#[test]
fn empaqueta_video_real() {
    let mp4 = sample_mp4(6);
    let frames = mestisexo_core::testing::split_frames(&mp4);
    assert!(frames.len() > 10, "se esperaban muchos fragmentos, hay {}", frames.len());
    assert_eq!(frames[0].0, mestisexo_core::testing::INIT, "el primero debe ser el init");
    assert!(
        frames.iter().filter(|(k, _)| *k == mestisexo_core::testing::FRAG_KEY).count() >= 5,
        "con -g 30 a 30 fps debería haber un keyframe por segundo"
    );

    let hls = mestisexo_core::testing::Packager::new();
    let mut started = 0;
    for (kind, data) in &frames {
        if hls.push_frame(*kind, data) {
            started += 1;
        }
    }
    assert_eq!(started, 1, "una sola señal, una sola sesión HLS");

    let m = hls.playlist();
    assert!(m.starts_with("#EXTM3U"), "{m}");
    for tag in [
        "#EXT-X-VERSION:9",
        "CAN-BLOCK-RELOAD=YES",
        "#EXT-X-PART-INF:PART-TARGET=",
        "#EXT-X-MAP:URI=\"init.mp4\"",
        "#EXT-X-PRELOAD-HINT:TYPE=PART",
    ] {
        assert!(m.contains(tag), "falta {tag} en:\n{m}");
    }
    assert!(m.contains("#EXT-X-PART:"), "sin partes:\n{m}");
    assert!(m.contains("#EXTINF:"), "sin segmentos cerrados:\n{m}");
    assert!(m.contains("INDEPENDENT=YES"), "ninguna parte marcada como independiente:\n{m}");

    // Cada parte anunciada tiene que poder descargarse, y no vacía.
    let anunciadas = partes(&m);
    assert!(anunciadas.len() >= 8, "pocas partes anunciadas: {}", anunciadas.len());
    for (msn, idx) in &anunciadas {
        let b = hls.part(*msn, *idx).unwrap_or_else(|| panic!("falta la parte {msn}/{idx}"));
        assert!(!b.is_empty(), "parte {msn}/{idx} vacía");
    }

    // Y cada segmento cerrado tiene que ser la concatenación exacta de sus partes.
    for msn in segmentos(&m) {
        let seg = hls.segment(msn).unwrap_or_else(|| panic!("falta el segmento {msn}"));
        let juntas: Vec<u8> = (0..)
            .map_while(|i| hls.part(msn, i))
            .flat_map(|p| p.to_vec())
            .collect();
        assert_eq!(seg.as_ref(), juntas.as_slice(), "el segmento {msn} no coincide con sus partes");
    }

    // La duración de una parte no puede pasar de PART-TARGET: Safari rechaza
    // la playlist si lo hace, y ffmpeg entrega fragmentos algo más largos que
    // los 0,25 s pedidos.
    let anunciado: f64 = m
        .lines()
        .find_map(|l| l.strip_prefix("#EXT-X-PART-INF:PART-TARGET="))
        .expect("falta PART-TARGET")
        .parse()
        .expect("PART-TARGET numérico");
    let durs = duraciones(&m);
    for d in &durs {
        assert!(*d <= anunciado, "parte de {d}s con PART-TARGET={anunciado}s");
    }

    // Las duraciones salen del propio MP4, no de la nominal: si la lectura de
    // `moof` fallara, todas serían exactamente 0,250.
    assert!(durs.iter().any(|d| (*d - 0.25).abs() > 0.001), "las duraciones parecen nominales: {durs:?}");
    for d in &durs {
        assert!(*d > 0.0 && *d < 1.0, "duración de parte fuera de rango: {d}");
    }

    // La ventana no crece sin fin.
    assert!(hls.segment_count() <= 6, "la ventana debería estar acotada: {}", hls.segment_count());
}

#[test]
fn el_init_abre_una_sesion_nueva() {
    let mp4 = sample_mp4(3);
    let frames = mestisexo_core::testing::split_frames(&mp4);
    let hls = mestisexo_core::testing::Packager::new();
    for (kind, data) in &frames {
        hls.push_frame(*kind, data);
    }
    let antes = hls.session();

    // El streamer reconecta: llega otro init y todo lo anterior deja de valer.
    for (kind, data) in &frames {
        hls.push_frame(*kind, data);
    }
    assert_ne!(antes, hls.session(), "una señal nueva debe estrenar URLs");
}

#[test]
fn un_segmento_solo_empieza_en_keyframe() {
    let mp4 = sample_mp4(4);
    let frames = mestisexo_core::testing::split_frames(&mp4);
    let hls = mestisexo_core::testing::Packager::new();
    // Se entrega el init y luego se salta el primer keyframe a propósito: hasta
    // que llegue otro no debe abrirse ningún segmento, porque nadie podría
    // decodificarlo.
    hls.push_frame(frames[0].0, &frames[0].1);
    let primer_key = frames.iter().position(|(k, _)| *k == mestisexo_core::testing::FRAG_KEY).expect("hay keyframe");
    for (kind, data) in frames.iter().skip(primer_key + 1).take(3) {
        assert_eq!(*kind, mestisexo_core::testing::FRAG_DELTA);
        hls.push_frame(*kind, data);
    }
    assert_eq!(hls.segment_count(), 0, "no se puede abrir un segmento sin keyframe");

    let siguiente = frames.iter().skip(primer_key + 1).find(|(k, _)| *k == mestisexo_core::testing::FRAG_KEY);
    let (kind, data) = siguiente.expect("hay un segundo keyframe");
    hls.push_frame(*kind, data);
    assert_eq!(hls.segment_count(), 1, "con keyframe sí se abre");
}

#[test]
fn el_token_caducado_no_vale() {
    let secret = b"una clave de al menos treinta y dos bytes";
    let ahora = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let bueno = mestisexo_core::sign_token(secret, ahora + 600).expect("firma");
    assert!(mestisexo_core::testing::token_ok(secret, Some(&bueno)));

    let caducado = mestisexo_core::sign_token(secret, ahora - 1).expect("firma");
    assert!(!mestisexo_core::testing::token_ok(secret, Some(&caducado)), "un token caducado no debe valer");

    assert!(!mestisexo_core::testing::token_ok(b"otra clave distinta de treinta y dos", Some(&bueno)), "otra clave no debe validar");
    assert!(!mestisexo_core::testing::token_ok(secret, None));
    assert!(!mestisexo_core::testing::token_ok(secret, Some("basura")));
    // Manipular la caducidad invalida la firma.
    let (payload, sig) = bueno.split_once('.').unwrap();
    let otro = mestisexo_core::sign_token(secret, ahora + 99999).unwrap();
    let falso = format!("{}.{sig}", otro.split_once('.').unwrap().0);
    assert!(!mestisexo_core::testing::token_ok(secret, Some(&falso)), "firma reusada con otra caducidad");
    assert!(!mestisexo_core::testing::token_ok(secret, Some(&format!("{payload}.{}", otro.split_once('.').unwrap().1))));
}

// ---------- Lectura de la playlist ----------

fn attr(line: &str, key: &str) -> Option<String> {
    let i = line.find(key)? + key.len();
    let rest = &line[i..];
    Some(rest.trim_start_matches('"').split(['"', ',']).next()?.to_string())
}

/// Partes anunciadas, como (msn, índice), leídas de la URI.
fn partes(m: &str) -> Vec<(u64, usize)> {
    m.lines()
        .filter(|l| l.starts_with("#EXT-X-PART:"))
        .filter_map(|l| {
            let uri = attr(l, "URI=")?;
            let p = uri.strip_prefix("part/")?.strip_suffix(".m4s")?;
            let (msn, idx) = p.split_once('/')?;
            Some((msn.parse().ok()?, idx.parse().ok()?))
        })
        .collect()
}

fn segmentos(m: &str) -> Vec<u64> {
    m.lines()
        .filter_map(|l| l.strip_prefix("seg/")?.strip_suffix(".m4s")?.parse().ok())
        .collect()
}

fn duraciones(m: &str) -> Vec<f64> {
    m.lines()
        .filter(|l| l.starts_with("#EXT-X-PART:"))
        .filter_map(|l| attr(l, "DURATION=")?.parse().ok())
        .collect()
}
