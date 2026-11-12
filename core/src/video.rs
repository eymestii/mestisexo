use crate::{Out, Shared, FRAG_DELTA, FRAG_KEY, INIT};
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;

const MAX_BOX: u64 = 64 * 1024 * 1024;

/// Bucle supervisor: ffmpeg acepta una conexión de OBS, y cuando OBS corta
/// se vuelve a lanzar para esperar la siguiente.
pub async fn run(shared: Arc<Shared>, ffmpeg: String, rtmp_url: String) {
    loop {
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            // Sin búfer de entrada: cada paquete de OBS sale en cuanto llega.
            .args(["-fflags", "nobuffer", "-flags", "low_delay", "-listen", "1", "-i"])
            .arg(&rtmp_url)
            .args([
                "-map", "0:v:0?", "-map", "0:a:0?", "-c", "copy",
                // Fragmentos de 0,25 s (y siempre uno nuevo en cada keyframe): el vídeo
                // sale casi en cuanto se graba y no a golpes de un GOP entero.
                "-f", "mp4", "-frag_duration", "250000",
                "-movflags", "frag_keyframe+empty_moov+default_base_moof",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        // Si la app muere de golpe, que ffmpeg no se quede ocupando el puerto RTMP.
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                shared.set_status(format!("No se pudo ejecutar ffmpeg ({e}). ¿Está instalado?"));
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        shared.set_status("Esperando a OBS…");

        if let Some(err) = child.stderr.take() {
            let sh = shared.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    sh.set_status(format!("ffmpeg: {l}"));
                }
            });
        }

        if let Some(out) = child.stdout.take() {
            let mut out = BufReader::with_capacity(256 * 1024, out);
            let _ = pump(&shared, &mut out).await;
        }
        let _ = child.kill().await;

        let was_live = {
            let mut st = shared.lock();
            st.live_since = None;
            st.codec = None;
            let was = st.live();
            st.cache.clear();
            was
        };
        if was_live {
            shared.send_json(serde_json::json!({ "t": "offline" }));
            shared.set_status("OBS se desconectó. Esperando a OBS…");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn read_box<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<([u8; 4], Vec<u8>)> {
    let mut h = [0u8; 8];
    r.read_exact(&mut h).await?;
    let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
    let kind = [h[4], h[5], h[6], h[7]];
    let mut buf = h.to_vec();
    let size = match size32 {
        1 => {
            let mut l = [0u8; 8];
            r.read_exact(&mut l).await?;
            buf.extend_from_slice(&l);
            u64::from_be_bytes(l)
        }
        0 => return Err(io::Error::new(io::ErrorKind::InvalidData, "caja MP4 sin tamaño")),
        n => n as u64,
    };
    if size < buf.len() as u64 || size > MAX_BOX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "caja MP4 inválida"));
    }
    let start = buf.len();
    buf.resize(size as usize, 0);
    r.read_exact(&mut buf[start..]).await?;
    Ok((kind, buf))
}

async fn pump<R: AsyncRead + Unpin>(shared: &Shared, r: &mut R) -> io::Result<()> {
    let mut init: Vec<u8> = vec![INIT];
    let mut frag: Vec<u8> = Vec::new();
    loop {
        let (kind, data) = read_box(r).await?;
        match &kind {
            b"ftyp" => {
                init.truncate(1);
                init.extend_from_slice(&data);
            }
            b"moov" => {
                init.extend_from_slice(&data);
                let codec = codec_string(&data);
                let init = Arc::new(std::mem::replace(&mut init, vec![INIT]));
                let mut st = shared.lock();
                st.codec = Some(codec.clone());
                st.cache.push(&init);
                st.live_since = Some(crate::now_ms());
                st.status = format!("En directo ({codec})");
                shared.send_json(serde_json::json!({ "t": "live", "codec": codec }));
                let _ = shared.tx.send(Out::Bin(init, Instant::now()));
            }
            b"moof" => {
                frag.clear();
                frag.push(if starts_with_keyframe(&data) { FRAG_KEY } else { FRAG_DELTA });
                frag.extend_from_slice(&data);
            }
            b"mdat" if !frag.is_empty() => {
                frag.extend_from_slice(&data);
                let f = Arc::new(std::mem::take(&mut frag));
                let mut st = shared.lock();
                st.bytes_total += f.len() as u64;
                st.frags_total += 1;
                st.last_data_ms = crate::now_ms();
                st.cache.push(&f);
                let _ = shared.tx.send(Out::Bin(f, Instant::now()));
            }
            _ => {}
        }
    }
}

fn be32(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Recorre las cajas hijas de `b` y devuelve (tipo, contenido sin cabecera).
fn children(b: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let size = be32(b, i)? as usize;
        let kind = b.get(i + 4..i + 8)?;
        let body = b.get(i + 8..i + size).filter(|_| size >= 8)?;
        i += size;
        Some((kind, body))
    })
}

/// Indica si el primer sample de cada pista del `moof` es un keyframe, es
/// decir, si el fragmento se puede decodificar sin los anteriores.
fn starts_with_keyframe(moof: &[u8]) -> bool {
    const NON_SYNC: u32 = 0x0001_0000;
    let Some(body) = moof.get(8..) else { return false };
    let mut any = false;
    for (kind, traf) in children(body) {
        if kind != b"traf" {
            continue;
        }
        let mut default_flags = 0;
        let mut first_flags = None;
        for (kind, b) in children(traf) {
            let Some(fl) = be32(b, 0).map(|v| v & 0x00ff_ffff) else { continue };
            if kind == b"tfhd" {
                let mut i = 8;
                for (bit, len) in [(0x1, 8), (0x2, 4), (0x8, 4), (0x10, 4)] {
                    if fl & bit != 0 {
                        i += len;
                    }
                }
                if fl & 0x20 != 0 {
                    default_flags = be32(b, i).unwrap_or(0);
                }
            } else if kind == b"trun" {
                let mut i = 8 + if fl & 0x1 != 0 { 4 } else { 0 };
                if fl & 0x4 != 0 {
                    first_flags = be32(b, i);
                } else if fl & 0x400 != 0 {
                    i += if fl & 0x100 != 0 { 4 } else { 0 } + if fl & 0x200 != 0 { 4 } else { 0 };
                    first_flags = be32(b, i);
                }
            }
        }
        if first_flags.unwrap_or(default_flags) & NON_SYNC != 0 {
            return false;
        }
        any = true;
    }
    any
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Construye el string `codecs="…"` que necesita MediaSource a partir del moov.
fn codec_string(moov: &[u8]) -> String {
    let mut codecs = Vec::new();
    if let Some(i) = find(moov, b"avcC") {
        if let Some(p) = moov.get(i + 5..i + 8) {
            codecs.push(format!("avc1.{:02x}{:02x}{:02x}", p[0], p[1], p[2]));
        }
    } else if find(moov, b"hvcC").is_some() {
        codecs.push("hvc1.1.6.L120.B0".to_string());
    }
    if find(moov, b"mp4a").is_some() {
        codecs.push("mp4a.40.2".to_string());
    } else if find(moov, b"Opus").is_some() {
        codecs.push("opus".to_string());
    }
    codecs.join(",")
}
