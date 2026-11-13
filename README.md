# Mestisexo

Streaming en tiempo real con chat, en tres piezas:

- **Módulo del streamer** (Tauri 2 + Rust, solo Arch Linux): recibe la señal de OBS por RTMP y envía una sola copia al VPS por SSH.
- **VPS**: el repetidor (`mestisexo-repeater`) convierte esa copia en LL-HLS y la sirve por HTTP, el servicio de cuentas (`mestisexo-auth`) gestiona Discord y los pagos, y Caddy sirve todo por HTTPS.
- **Web para espectadores**: `https://109-123-252-126.sslip.io/`. Entra directo al directo y pide iniciar sesión con Discord (hay que ser miembro del servidor). Funciona en cualquier navegador, también en móviles (iPhone con iOS 17.1 o posterior), sin descargar nada.

El chat se guarda solo en memoria (máximo 200 mensajes), se borra al terminar la sesión y nunca se escribe en disco.

## Reparto del vídeo

El vídeo se entrega en **LL-HLS**: el repetidor agrupa los fragmentos que le llegan en trozos de 0,25 s (partes) y segmentos de 2 s, y los publica en una playlist que el reproductor pide por HTTP. El chat sigue por WebSocket en `/ws`, y es independiente: si se cae el chat, el vídeo no se corta.

La razón del cambio es la escala. Un trozo de vídeo tiene la misma URL para todos y no cambia nunca, así que se sirve con `Cache-Control: immutable` y cualquier CDN puesto delante del dominio lo entrega sin tocar el VPS. Solo la playlist va con `no-store`. Antes el vídeo salía por WebSocket y cada espectador obligaba al VPS a subir su propia copia, lo que ponía el techo en unos 75 espectadores por la subida del servidor.

Cada señal nueva estrena un identificador aleatorio en la ruta (`/hls/<sesión>/live.m3u8`), así que nada cacheado de un directo anterior se puede confundir con el de ahora.

El empaquetado lo hace el repetidor, no ffmpeg: el muxer HLS de ffmpeg (9.0.2) no sabe generar `EXT-X-PART` ni responder a recargas bloqueantes, que es justo lo que baja la latencia.

## Calidad de conexión del espectador

La web mide, en el propio elemento de vídeo, cuántos segundos tiene por delante en el búfer y cuántas veces se le ha cortado. Al espectador no se le muestra nada: la medida solo sirve para avisar al streamer. No cambia la calidad del vídeo, que es única para todos.

No se cuentan bytes a mano, como se hacía con el WebSocket: los trozos los descarga el reproductor y pueden venir de la caché del navegador o del CDN sin pasar por el código de la página. En Chrome y Firefox, hls.js sí da una estimación del ancho de banda, que se muestra al pasar el ratón por el aviso; Safari no la expone.

Cuando un espectador entra en estado «lenta» su navegador avisa al repetidor, y vuelve a avisar en cuanto se recupera. El repetidor solo cuenta esos cambios de estado, así que lo que ve el streamer es cuántos espectadores van mal **ahora**, no cuántos tuvieron un bache en algún momento. Quien cierra la pestaña estando mal también se descuenta.

En la ventana del streamer aparece entonces una píldora con el porcentaje de la audiencia con problemas y, al pasar el ratón, el ancho de banda medio de quien ha informado. La píldora solo aparece si hay alguien con problemas. Si el porcentaje se mantiene alto, baja el bitrate en OBS: sin transcodificación es la única palanca.

## Reproductor del espectador

El vídeo usa los controles normales del navegador: desde ahí se activa el sonido y se abre la pantalla completa.

Safari (macOS y iPhone) reproduce HLS por sí mismo, que gasta menos batería y da pantalla completa nativa. Chrome y Firefox no traen HLS, así que ahí se usa [hls.js](https://github.com/video-dev/hls.js). Va empaquetado con la web, fijado a una versión exacta en `ui/package.json` y con su hash de integridad en `ui/package-lock.json`, para que la política de contenido siga siendo `'self'` y el directo no dependa de que un CDN de terceros responda. Va en su propio fichero y solo lo descargan los navegadores que lo necesitan.

La latencia queda en **1,5-2,5 s**, frente a los 0,5 s del reparto por WebSocket. Es el precio de que el vídeo sea cacheable. El margen por detrás del directo ya no lo lleva esta página: lo gobierna la playlist (`PART-HOLD-BACK`) y lo gestiona el reproductor, que se retrasa solo si la red de ese espectador no da y vuelve a acercarse cuando puede.

## Módulo del streamer (Arch Linux)

```sh
cd packaging/arch && makepkg -si
```

Depende de `webkit2gtk-4.1`, `gst-plugins-base`, `gst-plugins-good`, `gst-libav`, `ffmpeg` y `openssh`; para compilar, además, `rust` y `npm`. Para probar sin instalar: `cargo tauri build --no-bundle` (compila la interfaz y la app), o `cargo tauri dev` para desarrollar con recarga en caliente.

## Interfaz (`ui/`)

La misma interfaz sirve para la ventana del streamer (Tauri) y para la web de los espectadores. Está hecha con Svelte 5 y TypeScript, y Vite la compila a ficheros estáticos en `ui/dist/`, que es lo que se instala en el VPS.

```sh
cd ui
npm ci
npm run dev      # http://localhost:5173, con recarga en caliente
npm run check    # tipos y avisos de Svelte
npm test         # pruebas de la lógica (vitest)
npm run build    # genera ui/dist/
```

`npm run dev` reenvía `/ws`, `/hls`, `/api` y `/login` a un repetidor y un servicio de cuentas locales (puertos 8090, 8094 y 8092). Con `MESTISEXO_BACKEND=https://109-123-252-126.sslip.io npm run dev` usa los del VPS, aunque el inicio de sesión con Discord solo vuelve al dominio real.

Cada pantalla va en su propio fichero: el espectador no descarga el código del streamer, ni el streamer el reproductor. Los ficheros de `assets/` llevan un hash del contenido en el nombre, así que Caddy los sirve como inmutables y cada versión nueva se descarga sola.

1. Abre Mestisexo, rellena «VPS» (`stream@109.123.252.126`) y «Clave SSH» (`~/.ssh/mestisexo_tunnel`), y pulsa **Iniciar sesión**.
2. En OBS: *Ajustes → Emisión → Servicio: Personalizado*. Pega el servidor y la clave de stream.
   En *Salida* usa **x264/NVENC H.264, AAC, perfil main o high, intervalo de fotogramas clave 2 s**.
3. Comparte el «Enlace para espectadores».

Durante el directo, tu ventana muestra solo el chat, el tiempo en directo y los espectadores. El vídeo no se te envía, así que tu PC no gasta en decodificarlo. Para ver cómo queda, abre el enlace en un navegador.

## VPS

Tu PC envía **una sola copia** de la señal al VPS por SSH, y el repetidor la empaqueta en LL-HLS para todos los espectadores. Tu subida solo necesita el bitrate de OBS, haya los espectadores que haya. Tu IP no queda expuesta.

Cada release incluye `mestisexo-vps-x86_64-linux.tar.gz` con los binarios estáticos, la carpeta `web/` y los ficheros de `deploy/`. En el VPS (como root):

```sh
mkdir -p /tmp/mestisexo && tar -xzf mestisexo-vps-x86_64-linux.tar.gz -C /tmp/mestisexo && cd /tmp/mestisexo
install -m 755 mestisexo-repeater mestisexo-auth /usr/local/bin/
install -m 644 mestisexo-repeater.service mestisexo-auth.service /etc/systemd/system/
install -D -m 644 10-discord.conf /etc/systemd/system/mestisexo-repeater.service.d/10-discord.conf
install -m 644 50-mestisexo-stream.conf /etc/ssh/sshd_config.d/
install -m 644 Caddyfile /etc/caddy/Caddyfile
rm -rf /srv/mestisexo/web && mkdir -p /srv/mestisexo && cp -r web /srv/mestisexo/web
# Clave que firma el acceso al vídeo. Solo la primera vez.
mkdir -p /etc/mestisexo && umask 077 && printf 'HLS_SECRET=%s\n' "$(openssl rand -base64 48)" > /etc/mestisexo/hls.env
systemctl daemon-reload && systemctl enable --now mestisexo-auth mestisexo-repeater
sshd -t && systemctl reload ssh && systemctl reload caddy
ufw allow 80/tcp && ufw allow 443/tcp
```

En `~stream/.ssh/authorized_keys`, la línea de la clave debe permitir solo la conexión al repetidor:

```
restrict,port-forwarding,permitopen="127.0.0.1:8091" ssh-ed25519 AAAA… mestisexo
```

Todo escucha solo en `127.0.0.1`: el repetidor en `8090` (chat, a través de Caddy en `/ws`), `8091` (streamer, solo accesible por SSH) y `8094` (vídeo LL-HLS, a través de Caddy en `/hls/*`), y el servicio de cuentas en `8092` (público, a través de Caddy) y `8093` (interno). Desde fuera solo están abiertos el 80 y el 443.

### Cuentas de Discord y suscripciones (VPS)

Con el servicio `mestisexo-auth`, solo pueden ver el stream los miembros de tu servidor de Discord. En el chat aparecen con su nombre de Discord, y los suscriptores (2,99 $/mes, cobrados con Dodo Payments) llevan la insignia **SUB** y el rol «Suscriptor» en Discord. El rol se quita al cancelar la suscripción.

- Caddy sirve `https://109-123-252-126.sslip.io` con un certificado de Let's Encrypt y lo reenvía a `127.0.0.1:8092`.
- Los secretos solo están en el VPS, en `/etc/mestisexo/auth.env` (root, 600). La plantilla está en `deploy/auth.env.example`. Si una parte no tiene claves, queda desactivada.
- La web inicia sesión con Discord, que devuelve un código a la propia página. La página solo lo acepta si coincide con el `state` que guardó antes de salir. Para cada conexión al chat pide un ticket de un solo uso, que el repetidor canjea por la red interna (`127.0.0.1:8093`).
- El vídeo no puede usar ese ticket, porque el reproductor pide decenas de ficheros por minuto. En su lugar, la misma llamada devuelve un token firmado con HMAC-SHA256 y 10 minutos de validez, que el reproductor añade solo a la playlist. Al servirla, el origen deja una cookie `HttpOnly` atada a `/hls/<sesión>/`, y es la cookie la que autoriza el `init`, los segmentos y las partes: sus URLs quedan limpias e iguales para todos, así que un CDN las cachea sin mirar el token. El repetidor valida la firma con la clave compartida, sin consultar a nadie, y la web renueva el token antes de que caduque.
- Esa clave está en `/etc/mestisexo/hls.env` (root, 600; plantilla en `deploy/hls.env.example`), en un fichero aparte para que el repetidor no vea las claves de Discord ni las de pagos. La leen los dos servicios como `HLS_SECRET`, nunca por la línea de órdenes. Mínimo 32 caracteres. Sin ella, el vídeo queda accesible a quien tenga la URL y el repetidor lo avisa al arrancar.
- La web vive en `/srv/mestisexo/web` (es la carpeta `ui/dist/`, ya compilada). Caddy la sirve, reenvía `/ws` al repetidor y `/hls/*` al origen de vídeo, así que todo va cifrado.

### Tiempo real

- ffmpeg corta la señal en trozos de 0,25 s y los envía en cuanto salen, sin búfer de entrada. Cada trozo es una parte (`EXT-X-PART`) de la playlist, y los que hay entre fotogramas clave forman un segmento de unos 2 s.
- Si el VPS lleva más de 1 s de retraso respecto al PC del streamer, se le descarta el vídeo atrasado y salta al siguiente fotograma clave. Nunca se le manda lo atrasado.
- La playlist anuncia `CAN-BLOCK-RELOAD=YES` y `EXT-X-PRELOAD-HINT`: el reproductor pide la parte siguiente antes de que exista y el repetidor le deja la petición abierta hasta que ffmpeg la entrega (como mucho 5 s), en vez de hacerle preguntar «¿ya está?» cada poco. Eso es lo que mantiene la latencia en 1,5-2,5 s. Caddy no limita esa espera por defecto.
- Se mantienen los 6 últimos segmentos descargables, y las partes solo de los 3 últimos. Al terminar el directo, la sesión desaparece y lo cacheado deja de servirse.
- En OBS usa **intervalo de fotogramas clave de 2 s** y el preset de baja latencia (x264: `tune=zerolatency`). Tiene que coincidir con la duración del segmento: con 1 s habría un segmento por segundo y el doble de peticiones sin ganar latencia.

## Seguridad

- El RTMP solo escucha en `127.0.0.1`, así que OBS tiene que correr en el mismo equipo.
- En la web, la cuenta de Discord es lo que da acceso: no hace falta clave. La clave de stream de OBS es fija entre sesiones y se puede cambiar con «Nueva».
- Todo va cifrado: los espectadores por HTTPS (vídeo y web) y wss (chat), y el streamer por SSH.
- Límites del chat: 300 caracteres por mensaje y un mensaje cada 0,7 s por persona. El repetidor admite 500 conexiones de chat como máximo (`--max-viewers`); el vídeo no tiene ese límite, porque lo sirve el origen HTTP.

### Capacidad del VPS

Con el vídeo por HTTP, el techo lo marca lo que el CDN no pueda absorber. Las cifras de abajo se midieron con el reparto por WebSocket, cuando el VPS subía una copia por espectador:

| Espectadores | RAM (Caddy + repetidor) | CPU (núcleos) | Subida necesaria |
|---|---|---|---|
| 100 | 62 MB | 0,14 | 250 Mbit/s |
| 250 | 99 MB | 0,36 | 625 Mbit/s |
| 500 | 155 MB | 0,70 | 1,25 Gbit/s |

La columna de CPU son núcleos enteros, no fracción de los 4 del VPS: se midió con el repetidor en un solo hilo, así que 0,70 era un núcleo al 70 % con los otros tres sin usar.

La RAM y la CPU sobraban; el límite era la red. El VPS sube unos 200 Mbit/s, que daba para unos 75 espectadores a 2,5 Mbit/s. Con LL-HLS, cada trozo se sirve una vez al CDN y el CDN lo reparte, así que esa subida deja de ser el techo. Sin CDN delante, el límite sigue siendo el mismo.
