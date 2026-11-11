use crate::Shared;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Túnel SSH inverso hacia un VPS: los espectadores se conectan al VPS y el
/// tráfico llega a este equipo sin exponer su IP. Se reconecta solo.
pub struct Tunnel {
    /// Destino SSH, p. ej. `stream@109.123.252.126`.
    pub target: String,
    /// Clave privada a usar (opcional).
    pub identity: Option<String>,
    /// Puerto público en el VPS.
    pub remote_port: u16,
}

pub async fn run(shared: Arc<Shared>, t: Tunnel, local_port: u16) {
    loop {
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-N", "-T",
            "-o", "BatchMode=yes",
            "-o", "ExitOnForwardFailure=yes",
            "-o", "ServerAliveInterval=10",
            "-o", "ServerAliveCountMax=3",
            "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=accept-new",
        ]);
        if let Some(id) = &t.identity {
            cmd.args(["-o", "IdentitiesOnly=yes", "-i", id]);
        }
        cmd.arg("-R")
            .arg(format!("0.0.0.0:{}:127.0.0.1:{local_port}", t.remote_port))
            .arg("--")
            .arg(&t.target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
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
            Err(e) => set(&shared, format!("No se pudo ejecutar ssh: {e}")),
            Ok(mut child) => {
                set(&shared, format!("Conectando con {}…", t.target));
                let err = child.stderr.take();
                let sh = shared.clone();
                let target = t.target.clone();
                // ssh no avisa al conectar; si sigue vivo tras unos segundos, el túnel está arriba.
                let mark = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    set(&sh, format!("Público en {target}"));
                });
                let mut last = String::new();
                if let Some(err) = err {
                    let mut lines = BufReader::new(err).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        if !l.trim().is_empty() {
                            last = l;
                        }
                    }
                }
                let _ = child.wait().await;
                mark.abort();
                set(&shared, if last.is_empty() { "Túnel caído, reconectando…".into() } else { format!("Túnel caído ({last}), reconectando…") });
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn set(shared: &Shared, s: String) {
    shared.lock().tunnel_status = Some(s);
}
