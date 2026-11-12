# Mestisexo

Streaming en tiempo real con chat, en tres piezas:

- **Módulo del streamer** (Tauri 2 + Rust, solo Arch Linux): recibe la señal de OBS por RTMP y envía una sola copia al VPS por SSH.
- **VPS**: el repetidor (`mestisexo-repeater`) reparte esa copia a todos los espectadores, el servicio de cuentas (`mestisexo-auth`) gestiona Discord y los pagos, y Caddy sirve todo por HTTPS.
- **Web para espectadores**: `https://109-123-252-126.sslip.io/`. Entra directo al directo y pide iniciar sesión con Discord (hay que ser miembro del servidor). Funciona en cualquier navegador, también en móviles (iPhone con iOS 17.1 o posterior), sin descargar nada.

El chat se guarda solo en memoria (máximo 200 mensajes), se borra al terminar la sesión y nunca se escribe en disco.

## Calidad de conexión del espectador

La web mide el ancho de banda que recibe cada espectador (bytes por segundo del propio stream) y lo que le queda en el búfer, y lo resume en una etiqueta en la cabecera: «Buena conexión», «Conexión media» o «Conexión lenta». Es informativo: no cambia la calidad del vídeo, que es única para todos.

Cuando un espectador baja de 1 Mbit/s, su navegador manda un aviso al repetidor, que lleva la cuenta y la pasa al streamer. En la ventana del streamer aparece entonces una píldora con el porcentaje de espectadores con problemas y, al pasar el ratón, el ancho de banda medio de la audiencia. Si ese porcentaje sube, baja el bitrate en OBS.

Dos limitaciones de esa píldora: los contadores son acumulativos desde que arrancó la sesión, así que un espectador que tuvo un bache al principio sigue contando durante horas, y solo informan los espectadores con problemas, por lo que el porcentaje queda sesgado hacia arriba. Sirve para ver una tendencia, no como medida exacta.

## Reproductor del espectador

Encima del vídeo no se dibuja nada: ni barra de progreso (en un directo no hay nada que buscar) ni controles. Un clic sobre el vídeo activa o silencia el sonido, y un doble clic abre la pantalla completa.

El reproductor va un poco por detrás del directo para absorber los baches de la red, y cuánto va depende de la red de cada espectador. Arranca en 0,5 s. Cada vez que el navegador se queda sin datos (evento `waiting`) el margen sube medio segundo, hasta un tope de 3 s; tras un minuto sin un solo corte baja un cuarto de segundo cada 30 s, hasta volver a 0,5 s. El valor aprendido se guarda en `localStorage`, así que quien tiene mala conexión arranca con holgura en vez de descubrirla a base de cortes.

Sube rápido y baja despacio a propósito. Si subiera y bajara con la misma facilidad oscilaría, y a quien tiene una red regular le interesa más mantener el margen que recuperar medio segundo de cercanía al directo.

Con buena conexión nada de esto se activa: el margen se queda en 0,5 s. Es el mismo canje que hacen las plataformas grandes, que van a unos 3 s del directo precisamente para no cortarse.

## Módulo del streamer (Arch Linux)

```sh
cd packaging/arch && makepkg -si
```

Depende de `webkit2gtk-4.1`, `gst-plugins-base`, `gst-plugins-good`, `gst-libav`, `ffmpeg` y `openssh`. Para probar sin instalar: `cargo build --release -p mestisexo`.

1. Abre Mestisexo, rellena «VPS» (`stream@109.123.252.126`) y «Clave SSH» (`~/.ssh/mestisexo_tunnel`), y pulsa **Iniciar sesión**.
2. En OBS: *Ajustes → Emisión → Servicio: Personalizado*. Pega el servidor y la clave de stream.
   En *Salida* usa **x264/NVENC H.264, AAC, perfil main o high, intervalo de fotogramas clave 1 s**.
3. Comparte el «Enlace para espectadores».

Durante el directo, tu ventana muestra solo el chat y el estado (OBS, kbps, VPS). El vídeo no se te envía, así que tu PC no gasta en decodificarlo. Para ver cómo queda, abre el enlace en un navegador.

## VPS

Tu PC envía **una sola copia** de la señal al VPS por SSH, y el repetidor la reparte a todos los espectadores. Tu subida solo necesita el bitrate de OBS, haya los espectadores que haya. Tu IP no queda expuesta.

Cada release incluye `mestisexo-vps-x86_64-linux.tar.gz` con los binarios estáticos, la carpeta `web/` y los ficheros de `deploy/`. En el VPS (como root):

```sh
mkdir -p /tmp/mestisexo && tar -xzf mestisexo-vps-x86_64-linux.tar.gz -C /tmp/mestisexo && cd /tmp/mestisexo
install -m 755 mestisexo-repeater mestisexo-auth /usr/local/bin/
install -m 644 mestisexo-repeater.service mestisexo-auth.service /etc/systemd/system/
install -D -m 644 10-discord.conf /etc/systemd/system/mestisexo-repeater.service.d/10-discord.conf
install -m 644 50-mestisexo-stream.conf /etc/ssh/sshd_config.d/
install -m 644 Caddyfile /etc/caddy/Caddyfile
rm -rf /srv/mestisexo/web && mkdir -p /srv/mestisexo && cp -r web /srv/mestisexo/web
systemctl daemon-reload && systemctl enable --now mestisexo-auth mestisexo-repeater
sshd -t && systemctl reload ssh && systemctl reload caddy
ufw allow 80/tcp && ufw allow 443/tcp
```

En `~stream/.ssh/authorized_keys`, la línea de la clave debe permitir solo la conexión al repetidor:

```
restrict,port-forwarding,permitopen="127.0.0.1:8091" ssh-ed25519 AAAA… mestisexo
```

Todo escucha solo en `127.0.0.1`: el repetidor en `8090` (espectadores, a través de Caddy en `/ws`) y `8091` (streamer, solo accesible por SSH), y el servicio de cuentas en `8092` (público, a través de Caddy) y `8093` (interno). Desde fuera solo están abiertos el 80 y el 443.

### Cuentas de Discord y suscripciones (VPS)

Con el servicio `mestisexo-auth`, solo pueden ver el stream los miembros de tu servidor de Discord. En el chat aparecen con su nombre de Discord, y los suscriptores (2,99 $/mes, cobrados con Dodo Payments) llevan la insignia **SUB** y el rol «Suscriptor» en Discord. El rol se quita al cancelar la suscripción.

- Caddy sirve `https://109-123-252-126.sslip.io` con un certificado de Let's Encrypt y lo reenvía a `127.0.0.1:8092`.
- Los secretos solo están en el VPS, en `/etc/mestisexo/auth.env` (root, 600). La plantilla está en `deploy/auth.env.example`. Si una parte no tiene claves, queda desactivada.
- La web inicia sesión con Discord, que devuelve un código a la propia página. La página solo lo acepta si coincide con el `state` que guardó antes de salir. Para cada conexión al stream pide un ticket de un solo uso, que el repetidor canjea por la red interna (`127.0.0.1:8093`).
- La web vive en `/srv/mestisexo/web` (es la carpeta `ui/`). Caddy la sirve y reenvía `/ws` al repetidor, así que el vídeo va cifrado (wss).

### Tiempo real

- ffmpeg corta la señal en trozos de 0,25 s y los envía en cuanto salen, sin búfer de entrada.
- Si un destino (el VPS o un espectador) lleva más de 1 s de retraso, se le descarta el vídeo atrasado y salta al siguiente fotograma clave. Nunca se le manda lo atrasado.
- El reproductor va por detrás del último dato recibido lo que la red de cada espectador necesite: 0,5 s con buena conexión, hasta 3 s si se le corta (ver «Margen del reproductor»). Si pasa de 0,4 s sobre su objetivo acelera un 5 % hasta recuperarlo, y si pasa de 1 s sobre el objetivo salta al directo. La velocidad no cambia más de una vez cada 2 s a propósito: cada cambio obliga al navegador a reprocesar el audio, y hacerlo seguido lo corta.
- Solo se guarda el último GOP (desde el último fotograma clave), para que quien entra vea imagen al momento.
- En OBS usa **intervalo de fotogramas clave de 1 s** y el preset de baja latencia (x264: `tune=zerolatency`). Cuanto más corto el intervalo, antes se recupera el vídeo tras un corte.

## Seguridad

- El RTMP solo escucha en `127.0.0.1`, así que OBS tiene que correr en el mismo equipo.
- En la web, la cuenta de Discord es lo que da acceso: no hace falta clave. La clave de stream de OBS es fija entre sesiones y se puede cambiar con «Nueva».
- Todo va cifrado: los espectadores por HTTPS/wss y el streamer por SSH.
- Límites del chat: 300 caracteres por mensaje y un mensaje cada 0,7 s por persona. El repetidor admite 500 espectadores como máximo (`--max-viewers`).

### Capacidad del VPS

Medido con el repetidor y Caddy reales y espectadores simulados por wss, a 2,5 Mbit/s:

| Espectadores | RAM (Caddy + repetidor) | CPU (de 4 núcleos) | Subida necesaria |
|---|---|---|---|
| 100 | 62 MB | 0,14 | 250 Mbit/s |
| 250 | 99 MB | 0,36 | 625 Mbit/s |
| 500 | 155 MB | 0,70 | 1,25 Gbit/s |

La RAM y la CPU sobran. El límite es la red: el VPS sube unos 200 Mbit/s, que da para unos 75 espectadores a 2,5 Mbit/s o unos 130 a 1,5 Mbit/s.
