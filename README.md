# Mestisexo

App de escritorio para hacer streaming sin depender de servidores externos (Tauri 2 + Rust, con HTML/JS sin frameworks en la interfaz).

- **Transmitir**: OBS envía la señal por RTMP a la app, que la reparte por WebSocket sin recodificar, directamente o a través de un repetidor en un VPS.
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
   En *Salida* usa **x264/NVENC H.264, AAC, perfil main o high, intervalo de fotogramas clave 1 s**.
3. Comparte el código (`ip:puerto#clave`). Los espectadores eligen **Ver un stream** y lo pegan.

### Espectadores en otras redes (VPS)

Tu PC envía **una sola copia** de la señal al VPS por SSH, y el repetidor del VPS (`mestisexo-repeater`) la reparte a todos los espectadores. Tu subida solo necesita el bitrate de OBS, haya los espectadores que haya. El límite pasa a ser la subida del VPS (bitrate × espectadores). Tu IP no queda expuesta.

En **Transmitir**, rellena «VPS para espectadores» (`stream@IP_DEL_VPS`) y «Clave SSH» (`~/.ssh/mestisexo_tunnel`).

Instalación en el VPS (como root):

```sh
# En tu PC: compilar el repetidor (binario estático, sin dependencias)
rustup target add x86_64-unknown-linux-musl
cargo build --release -p mestisexo-core --bin mestisexo-repeater --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/mestisexo-repeater deploy/* root@IP_DEL_VPS:/tmp/

# En el VPS
install -m 755 /tmp/mestisexo-repeater /usr/local/bin/
install -m 644 /tmp/mestisexo-repeater.service /etc/systemd/system/
install -m 644 /tmp/50-mestisexo-stream.conf /etc/ssh/sshd_config.d/
systemctl daemon-reload && systemctl enable --now mestisexo-repeater
sshd -t && systemctl reload ssh
ufw allow 8090/tcp
```

En `~stream/.ssh/authorized_keys`, la línea de la clave debe permitir solo la conexión al repetidor:

```
restrict,port-forwarding,permitopen="127.0.0.1:8091" ssh-ed25519 AAAA… mestisexo
```

El repetidor escucha a los espectadores en `0.0.0.0:8090` y al streamer en `127.0.0.1:8091`, que solo es accesible por SSH.

También puedes usar Tailscale o ZeroTier sin VPS. En ese caso tu PC envía una copia por espectador. Si Tailscale está conectado y no configuras un VPS, el código del stream usa tu IP de Tailscale.

### Cuentas de Discord y suscripciones (VPS)

Con el servicio `mestisexo-auth`, solo pueden ver el stream los miembros de tu servidor de Discord. En el chat aparecen con su nombre de Discord, y los suscriptores (2,99 $/mes, cobrados con Dodo Payments) llevan la insignia **SUB** y el rol «Suscriptor» en Discord. El rol se quita al cancelar la suscripción.

- Caddy sirve `https://109-123-252-126.sslip.io` con un certificado de Let's Encrypt y lo reenvía a `127.0.0.1:8092`.
- Los secretos solo están en el VPS, en `/etc/mestisexo/auth.env` (root, 600). La plantilla está en `deploy/auth.env.example`. Si una parte no tiene claves, queda desactivada.
- La app inicia sesión en el navegador del sistema y recibe un código en un puerto local. Para cada conexión al stream pide un ticket de un solo uso, que el repetidor canjea por la red interna (`127.0.0.1:8093`).
- Para exigir la cuenta en el repetidor: `install -D -m 644 deploy/10-discord.conf /etc/systemd/system/mestisexo-repeater.service.d/10-discord.conf && systemctl daemon-reload && systemctl restart mestisexo-repeater`.

### Tiempo real

- ffmpeg corta la señal en trozos de 0,25 s y los envía en cuanto salen, sin búfer de entrada.
- Si un destino (el VPS o un espectador) lleva más de 1 s de retraso, se le descarta el vídeo atrasado y salta al siguiente fotograma clave. Nunca se le manda lo atrasado.
- El reproductor va unos 0,5 s por detrás del último dato recibido. Si se retrasa, acelera un poco, y si pasa de 1,5 s salta al directo.
- Solo se guarda el último GOP (desde el último fotograma clave), para que quien entra vea imagen al momento.
- En OBS usa **intervalo de fotogramas clave de 1 s** y el preset de baja latencia (x264: `tune=zerolatency`). Cuanto más corto el intervalo, antes se recupera el vídeo tras un corte.

## Seguridad

- El RTMP solo escucha en `127.0.0.1`, así que OBS tiene que correr en el mismo equipo.
- Sin la clave del código no se puede conectar. Las claves son nuevas en cada sesión.
- El tráfico no va cifrado. Para cifrarlo, usa la VPN.
- Límites del chat: 300 caracteres por mensaje, un mensaje cada 0,7 s por persona y 100 espectadores como máximo.
