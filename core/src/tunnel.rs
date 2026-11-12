//! Enlace con el repetidor del VPS: se envía una sola copia de la señal por
//! SSH (`ssh -W` al puerto de subida del repetidor) y el VPS la reparte. Así la
//! IP del streamer no queda expuesta y su subida no depende de los espectadores.

use crate::repeater::{read_frame, write_frame, T_ACK, T_JSON, T_MEDIA};
use crate::{Gate, Out, Pass, Shared, MAX_LAG};
use std::collections::VecDeque;
use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader, BufWriter};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

pub struct Tunnel {
    /// Destino SSH, p. ej. `stream@109.123.252.126`.
    pub target: String,
    /// Clave privada a usar (opcional).
    pub identity: Option<String>,
    /// Puerto de subida del repetidor en el VPS (escucha solo en loopback).
    pub upstream_port: u16,
}

pub async fn run(shared: Arc<Shared>, t: Tunnel) {
    loop {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-T",
            "-o", "BatchMode=yes",
            "-o", "ServerAliveInterval=5",
            "-o", "ServerAliveCountMax=3",
            "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=accept-new",
            "-o", "Compression=no",
            "-o", "IPQoS=none",
        ]);
        if let Some(id) = &t.identity {
            cmd.args(["-o", "IdentitiesOnly=yes", "-i", id]);
        }
        cmd.arg("-W")
            .arg(format!("127.0.0.1:{}", t.upstream_port))
            .arg("--")
            .arg(&t.target)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        #[cfg(target_os = "linux")]
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }

        match cmd.spawn() {
            Err(e) => set(&shared, false, format!("No se pudo ejecutar ssh: {e}")),
            Ok(mut child) => {
                set(&shared, false, format!("Conectando con {}…", t.target));
                let last = Arc::new(std::sync::Mutex::new(String::new()));
                if let Some(err) = child.stderr.take() {
                    let last = last.clone();
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(err).lines();
                        while let Ok(Some(l)) = lines.next_line().await {
                            if !l.trim().is_empty() {
                                *last.lock().unwrap_or_else(|e| e.into_inner()) = l;
                            }
                        }
                    });
                }
                if let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) {
                    let _ = session(&shared, &t.target, stdin, stdout).await;
                }
                let _ = child.kill().await;
                let last = last.lock().unwrap_or_else(|e| e.into_inner()).clone();
                set(&shared, false, if last.is_empty() { "VPS caído, reconectando…".into() } else { format!("VPS caído ({last}), reconectando…") });
                shared.lock().relay_viewers = 0;
                shared.viewers_changed();
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn session(shared: &Arc<Shared>, target: &str, stdin: ChildStdin, stdout: ChildStdout) -> io::Result<()> {
    // Lectura: acuses de recibo del vídeo, chat de los espectadores del VPS y su recuento.
    let (ack_tx, ack_rx) = watch::channel(0u64);
    let sh = shared.clone();
    let target = target.to_string();
    let reader = tokio::spawn(async move {
        let mut r = BufReader::new(stdout);
        let mut first = true;
        while let Ok((kind, data)) = read_frame(&mut r).await {
            if first {
                first = false;
                set(&sh, true, format!("Repetidor activo en {target}"));
            }
            match kind {
                T_ACK if data.len() == 8 => {
                    let n = u64::from_be_bytes(data.try_into().unwrap_or_default());
                    let _ = ack_tx.send(n);
                }
                T_JSON => from_repeater(&sh, &data),
                _ => {}
            }
        }
    });
    let _guard = AbortOnDrop(reader);

    let mut w = BufWriter::with_capacity(256 * 1024, stdin);
    // Suscribirse antes de copiar el estado para no perder nada entre medias.
    let (mut rx, hello, frames) = {
        let st = shared.lock();
        let rx = shared.tx.subscribe();
        let hello = serde_json::json!({
            "t": "hello",
            "key": st.key,
            "history": &st.chat,
            "live": st.live(),
            "codec": st.codec,
            "viewers": st.total_viewers(),
        });
        (rx, hello, st.cache.frames())
    };
    write_frame(&mut w, T_JSON, hello.to_string().as_bytes()).await?;

    // Control de retraso del enlace: se apunta cuándo se envió cada mensaje de
    // vídeo y el repetidor confirma los que recibe. Si el más antiguo sin
    // confirmar lleva más de MAX_LAG, el enlace va atrasado y se descarta vídeo
    // hasta el siguiente keyframe en vez de acumularlo en SSH.
    let mut link = Link { w, inflight: VecDeque::new(), sent: 0 };
    for f in frames {
        link.media(&f).await?;
    }

    let mut gate = Gate::default();
    loop {
        let out = rx.recv().await;
        link.acked(*ack_rx.borrow());
        match out {
            Ok(Out::Text(t)) => write_frame(&mut link.w, T_JSON, t.as_bytes()).await?,
            Ok(Out::Bin(b, at)) => match gate.pass(b[0], at.elapsed() > MAX_LAG || link.late()) {
                Pass::Skip => {}
                Pass::Send => link.media(&b).await?,
                Pass::InitThenSend => {
                    let init = shared.lock().cache.init.clone();
                    if let Some(init) = init {
                        link.media(&init).await?;
                    }
                    link.media(&b).await?;
                }
            },
            Ok(Out::Kick) => {}
            Err(RecvError::Lagged(_)) => gate.lost(true),
            Err(RecvError::Closed) => return Ok(()),
        }
    }
}

/// Control de retraso del enlace: se apunta cuándo se envió cada mensaje de
/// vídeo y el repetidor confirma los que recibe. Si el más antiguo sin
/// confirmar lleva más de MAX_LAG, el enlace va atrasado y se descarta vídeo
/// hasta el siguiente keyframe en vez de acumularlo en SSH.
struct Link {
    w: BufWriter<ChildStdin>,
    inflight: VecDeque<Instant>,
    sent: u64,
}

impl Link {
    async fn media(&mut self, b: &[u8]) -> io::Result<()> {
        self.inflight.push_back(Instant::now());
        self.sent += 1;
        write_frame(&mut self.w, T_MEDIA, b).await
    }

    fn acked(&mut self, acked: u64) {
        let confirmed_before = self.sent - self.inflight.len() as u64;
        let n = acked.saturating_sub(confirmed_before) as usize;
        self.inflight.drain(..n.min(self.inflight.len()));
    }

    fn late(&self) -> bool {
        self.inflight.front().is_some_and(|t| t.elapsed() > MAX_LAG)
    }
}

fn from_repeater(shared: &Shared, data: &[u8]) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else { return };
    match v["t"].as_str() {
        Some("chat") => {
            let name = crate::ws::clean(v["name"].as_str().unwrap_or(""), crate::ws::MAX_NAME);
            let text = crate::ws::clean(v["text"].as_str().unwrap_or(""), crate::ws::MAX_TEXT);
            if !text.is_empty() {
                let name = if name.is_empty() { "Anónimo".into() } else { name };
                shared.push_chat(name, text, false);
            }
        }
        Some("viewers") => {
            shared.lock().relay_viewers = v["n"].as_u64().unwrap_or(0) as usize;
            shared.viewers_changed();
        }
        _ => {}
    }
}

fn set(shared: &Shared, up: bool, s: String) {
    let mut st = shared.lock();
    st.tunnel_up = Some(up);
    st.tunnel_status = Some(s);
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
