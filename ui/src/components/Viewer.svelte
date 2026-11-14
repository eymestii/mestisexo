<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import Chat from "./Chat.svelte";
  import { ChatConnection, errorText } from "../lib/chat.svelte";
  import { AuthError, authApi, finishWebLogin, getToken, openUrl, startLogin } from "../lib/auth";
  import { Player } from "../lib/player";
  import { NetworkMonitor } from "../lib/quality.svelte";
  import type { Me, ServerMsg, Ticket } from "../lib/types";

  // Espectador: se entra directo al directo. El nombre sale de la cuenta de
  // Discord y el repetidor pide iniciar sesión si hace falta.

  let video = $state<HTMLVideoElement>();
  let live = $state(false);
  let placeholder = $state("Esperando señal…");
  let account = $state<Me | null>(null);
  let login = $state<{ msg: string; invite: string | null } | null>(null);

  // El servicio de cuentas está en el mismo dominio que la web, salvo que el
  // repetidor anuncie otro.
  let authUrl = location.origin;
  const viewerBase = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?v=1`;
  let loginError: string | null = null;
  let subTimer: ReturnType<typeof setInterval> | undefined;

  // ---------- Acceso al vídeo ----------
  // La playlist pide un token con caducidad; el ticket de un solo uso del chat
  // no sirve aquí, porque el reproductor pide decenas de ficheros por minuto.
  // El origen deja además una cookie atada a la sesión, y es la cookie la que
  // autoriza los trozos: sus URLs no llevan nada y las puede cachear un CDN.

  let livePath: string | null = null; // /hls/<sesión>/live.m3u8, lo anuncia el repetidor
  let videoToken: string | null = null;
  let videoExp = 0; // caducidad del token, en segundos desde epoch
  let videoTimer: ReturnType<typeof setTimeout> | undefined;
  let lastRefresh = 0;

  const chat = new ChatConnection({
    onLive: setLive,
    onAuth: onAuthRequired,
    onUrlError(e) {
      if (!(e instanceof AuthError)) return false;
      authNeeded(e);
      return true;
    },
  });

  const player = new Player({
    refresh: refreshVideo,
    unsupported: () => (placeholder = "Este navegador no puede reproducir el directo. Actualiza a iOS 17.1 o posterior, o usa otro navegador."),
    mutedAutoplay: () => chat.sys("El navegador empezó el vídeo sin sonido: actívalo con el control de volumen del vídeo."),
  });

  // No se le muestra al espectador: solo informa al streamer de cuántos van mal.
  const monitor = new NetworkMonitor({
    video: () => video ?? null,
    active: () => !!player.url,
    bandwidth: () => player.bandwidth(),
    report: (bandwidth, poor) => chat.send({ t: "quality_hint", bandwidth, poor }),
  });

  /** Token vigente, pidiendo uno nuevo si el que hay está por caducar. */
  async function videoAccess(): Promise<string | null> {
    if (videoToken && videoExp - 60 > Date.now() / 1000) return videoToken;
    const t = await authApi<Ticket>(authUrl, "/api/ticket", "POST");
    videoToken = t.video || null;
    videoExp = t.video_exp || 0;
    return videoToken;
  }

  /** Arranca el reproductor con la señal que anuncia el repetidor. */
  async function startVideo() {
    if (!livePath) return;
    const path = livePath;
    let token: string | null;
    try {
      token = await videoAccess();
    } catch (e) {
      if (e instanceof AuthError) return authNeeded(e);
      placeholder = "No se pudo pedir permiso para el vídeo. Reintentando…";
      clearTimeout(videoTimer);
      videoTimer = setTimeout(() => livePath === path && startVideo(), 3000);
      return;
    }
    // Mientras se pedía el token pudo empezar otra señal o irse el espectador.
    if (livePath !== path) return;
    placeholder = "";
    player.start(location.origin + path + (token ? `?t=${encodeURIComponent(token)}` : ""));
    monitor.start();
    // Se renueva antes de que caduque: si no, el reproductor se queda sin
    // poder pedir la playlist y el directo se corta.
    clearTimeout(videoTimer);
    if (videoExp) videoTimer = setTimeout(refreshVideo, Math.max(5000, (videoExp - 60) * 1000 - Date.now()));
  }

  // Vuelve a empezar con un token nuevo. Lo llama el reloj de renovación y
  // también un error de red del reproductor, que casi siempre es el token
  // caducado. Con un espaciado mínimo: si el repetidor está caído, los errores
  // llegan en ráfaga y sin esto se pediría un token tras otro.
  function refreshVideo() {
    if (!livePath) return;
    const wait = 3000 - (Date.now() - lastRefresh);
    clearTimeout(videoTimer);
    if (wait > 0) {
      videoTimer = setTimeout(refreshVideo, wait);
      return;
    }
    lastRefresh = Date.now();
    videoToken = null;
    videoExp = 0;
    player.destroy();
    startVideo();
  }

  // `hls` es la ruta de la playlist de la señal actual, que el repetidor manda
  // en el «welcome» y en cada «live». Cambia con cada señal nueva, así que nada
  // cacheado de un directo anterior se puede confundir con el de ahora.
  function setLive(on: boolean, hls?: string | null) {
    live = on;
    placeholder = on ? "" : "Esperando señal…";
    if (on && hls) {
      livePath = hls;
      startVideo();
    }
    if (!on) {
      livePath = null;
      monitor.stop();
      player.destroy();
    }
  }

  // ---------- Cuentas de Discord y suscripción ----------
  // Si el repetidor exige cuenta, responde {t:"auth", url} y cierra. La web
  // inicia sesión en esa dirección, guarda el token de sesión y pide un ticket
  // de un solo uso para cada conexión.

  async function viewerUrl(): Promise<string> {
    // Una misma llamada da las dos cosas: el ticket de un solo uso para el chat
    // y el token con caducidad para el vídeo.
    const [t, me] = await Promise.all([
      authApi<Ticket>(authUrl, "/api/ticket", "POST"),
      authApi<Me>(authUrl, "/api/me"),
    ]);
    videoToken = t.video || null;
    videoExp = t.video_exp || 0;
    account = me;
    return `${viewerBase}&t=${t.ticket}`;
  }

  function onAuthRequired(msg: Extract<ServerMsg, { t: "auth" }>) {
    if (typeof msg.url !== "string" || !msg.url.startsWith("https://")) return;
    chat.drop();
    if (authUrl !== msg.url) {
      authUrl = msg.url;
      // Primera vez: con la URL del servicio ya se puede pedir un ticket.
      if (getToken(authUrl) && !msg.retry) {
        chat.connect(viewerUrl);
        return;
      }
    }
    authNeeded(new AuthError("session"));
  }

  function authNeeded(e: AuthError) {
    chat.forget();
    account = null;
    setLive(false);
    placeholder = "";
    let msg = e.code === "not_member"
      ? "Para ver este stream tienes que estar en el servidor de Discord. Únete y vuelve a iniciar sesión."
      : "Este stream es solo para miembros del servidor de Discord.";
    if (loginError) {
      msg = loginError;
      loginError = null;
    }
    login = { msg, invite: e.code === "not_member" ? e.invite : null };
  }

  async function subscribe() {
    try {
      const { url } = await authApi<{ url: string }>(authUrl, "/api/checkout", "POST");
      openUrl(url);
      chat.sys("Completa el pago en el navegador. La insignia aparecerá al confirmarse.");
      // Al confirmarse el pago, reconectar para entrar con la insignia.
      clearInterval(subTimer);
      let tries = 0;
      subTimer = setInterval(async () => {
        if (++tries > 120) return clearInterval(subTimer);
        const me = await authApi<Me>(authUrl, "/api/me").catch(() => null);
        if (me?.sub) {
          clearInterval(subTimer);
          account = me;
          chat.sys("¡Gracias por suscribirte!");
          chat.reconnect();
        }
      }, 5000);
    } catch (e) {
      if (e instanceof AuthError) authNeeded(e);
      else chat.sys(errorText(e));
    }
  }

  function onStall() {
    if (player.url) monitor.onStall();
  }

  onMount(async () => {
    player.video = video!;
    document.body.classList.add("web");
    localStorage.removeItem("web-key");
    loginError = await finishWebLogin(authUrl);
    chat.connect(viewerUrl);
  });

  onDestroy(() => {
    chat.stop();
    clearInterval(subTimer);
    clearTimeout(videoTimer);
    monitor.stop();
    player.destroy();
  });
</script>

<section id="live">
  <header>
    <span class="badge" class:on={live}>{live ? "EN VIVO" : "OFFLINE"}</span>
    <span id="viewers" title="Espectadores">{chat.viewers}</span>
    {#if account}
      <div class="account">
        {#if account.sub}<span class="sub" title="Suscriptor">SUB</span>{/if}
        <b>{account.name}</b>
        {#if !account.sub && account.payments}
          <button class="primary" onclick={subscribe}>Suscribirse · 2,99 $/mes</button>
        {/if}
      </div>
    {/if}
  </header>
  <div class="main">
    <div class="left">
      <div class="player">
        <video
          id="video"
          bind:this={video}
          autoplay
          playsinline
          controls
          onwaiting={onStall}
          onstalled={onStall}
        ></video>
        {#if placeholder}<div id="placeholder">{placeholder}</div>{/if}
        {#if login}
          <div class="login">
            <p>{login.msg}</p>
            <button class="primary" onclick={() => startLogin(authUrl)}>Iniciar sesión con Discord</button>
            {#if login.invite}
              <button onclick={() => openUrl(login!.invite!)}>Unirme al servidor</button>
            {/if}
          </div>
        {/if}
      </div>
    </div>
    <Chat {chat}>
      <div class="chat-head">
        <b>Con el apoyo de <a href="https://roky.cloud" target="_blank" rel="noopener">roky.cloud</a></b>
      </div>
    </Chat>
  </div>
</section>
