# Mestisexo

App de escritorio para hacer streaming sin depender de servidores externos (Tauri 2 + Rust, con HTML/JS sin frameworks en la interfaz).

- **Transmitir**: OBS envía la señal por RTMP a la app, que la reparte a los espectadores por WebSocket sin recodificar.
- **Ver**: otra persona con la app abre el código del stream y ve el vídeo y el chat.
- **Chat**: se guarda solo en memoria (máximo 200 mensajes). Se borra al terminar la sesión y nunca se escribe en disco.

## Requisitos

- `ffmpeg` en el PATH, o un `ffmpeg` (`ffmpeg.exe` en Windows) junto al ejecutable. Solo lo necesita el streamer.
- Linux: `webkit2gtk-4.1`, `gst-plugins-base`, `gst-plugins-good`, `gst-libav`.

## Compilar

```sh
cargo build --release -p mestisexo      # binario: target/release/mestisexo
cargo install tauri-cli && cd app && cargo tauri build   # instaladores (.deb, AppImage, .exe)
```

## Uso

1. **Transmitir → Iniciar sesión.** La app muestra el servidor RTMP, la clave de stream y el código para espectadores.
2. En OBS: *Ajustes → Emisión → Servicio: Personalizado*. Pega el servidor y la clave.
   En *Salida* usa **x264/NVENC H.264, AAC, perfil main o high, intervalo de fotogramas clave 1–2 s**.
3. Comparte el código (`ip:puerto#clave`). Los espectadores eligen **Ver un stream** y lo pegan.

### Espectadores en otras redes (VPS)

La app abre un túnel SSH inverso hacia el VPS. Los espectadores se conectan a la IP del VPS y el tráfico llega a tu PC por el túnel, así tu IP no queda expuesta.

En **Transmitir**, rellena «VPS para espectadores» (`stream@IP_DEL_VPS`) y «Clave SSH» (`~/.ssh/mestisexo_tunnel`). Así está configurado el VPS:

- Usuario `stream`, sin shell. Su clave en `authorized_keys` solo permite publicar `0.0.0.0:8090`.
- `/etc/ssh/sshd_config.d/50-mestisexo-stream.conf`: `GatewayPorts clientspecified` y `PermitListen 0.0.0.0:8090`, solo para ese usuario.
- `ufw allow 8090/tcp`.

Todo el vídeo sale de tu PC hacia el VPS una vez por espectador, así que tu subida necesita el bitrate de OBS × el número de espectadores. El VPS solo reenvía el tráfico.

También puedes usar Tailscale o ZeroTier. Si Tailscale está conectado y no configuras un VPS, el código del stream usa tu IP de Tailscale.

## Seguridad

- El RTMP solo escucha en `127.0.0.1`, así que OBS tiene que correr en el mismo equipo.
- Sin la clave del código no se puede conectar. Las claves son nuevas en cada sesión.
- El tráfico no va cifrado. Para cifrarlo, usa la VPN.
- Límites del chat: 300 caracteres por mensaje, un mensaje cada 0,7 s por persona y 100 espectadores como máximo.
