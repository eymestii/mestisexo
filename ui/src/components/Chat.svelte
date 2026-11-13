<script lang="ts">
  import type { Snippet } from "svelte";
  import { colorFor, type ChatConnection } from "../lib/chat.svelte";

  let { chat, children }: { chat: ChatConnection; children?: Snippet } = $props();

  let list = $state<HTMLUListElement>();
  let text = $state("");
  let stick = true;

  // Atajos que se expanden al enviar, solo como palabra suelta: «rip» sí,
  // «ripio» o «rip.com» no. El servidor recibe texto normal.
  const SHORTCUTS: Record<string, string> = {
    gg: "GG 🎮", wp: "WP 💪", lol: "LOL 😂", oof: "OOF 💀", hype: "HYPE 🔥", rip: "RIP ⚰️",
  };
  const SHORTCUT_RE = /(?<![\p{L}\p{N}_.\/:@#-])(gg|wp|lol|oof|hype|rip)(?![\p{L}\p{N}_\/:@#-]|\.[\p{L}\p{N}])/giu;
  const expand = (s: string) => s.replace(SHORTCUT_RE, (w) => SHORTCUTS[w.toLowerCase()]);

  // Solo se baja al último mensaje si ya se estaba abajo: quien sube a leer no
  // pierde el sitio.
  $effect.pre(() => {
    void chat.seq;
    if (list) stick = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  });
  $effect(() => {
    void chat.seq;
    if (list && stick) list.scrollTop = list.scrollHeight;
  });

  function submit(e: SubmitEvent) {
    e.preventDefault();
    const raw = text.trim();
    // El servidor corta a 300 caracteres: si al expandir se pasa, va tal cual.
    let t = expand(raw);
    if ([...t].length > 300) t = raw;
    if (t && chat.send({ t: "chat", text: t })) text = "";
  }
</script>

<aside class="chat">
  <div class="chat-head">
    <b>Con el apoyo de <a href="https://roky.cloud" target="_blank" rel="noopener">roky.cloud</a></b>
  </div>
  {@render children?.()}
  <ul id="messages" bind:this={list}>
    {#each chat.lines as line (line.id)}
      {#if line.sys}
        <li class="sys">{line.text}</li>
      {:else}
        <li class:host={line.m.host} title={new Date(line.m.ts).toLocaleTimeString()}>
          {#if line.m.sub}<span class="sub" title="Suscriptor">SUB</span>{/if}<span
            class="n"
            style:color={colorFor(line.m.name)}>{line.m.name}:</span>{line.m.text}
        </li>
      {/if}
    {/each}
  </ul>
  <form id="chat-form" onsubmit={submit}>
    <input bind:value={text} maxlength="300" placeholder="Escribe un mensaje" autocomplete="off">
    <button class="send">Enviar</button>
  </form>
</aside>
