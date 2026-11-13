<script lang="ts">
  import type { Snippet } from "svelte";
  import { colorFor, type ChatConnection } from "../lib/chat.svelte";

  let { chat, children }: { chat: ChatConnection; children?: Snippet } = $props();

  let list = $state<HTMLUListElement>();
  let text = $state("");
  let stick = true;

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
    const t = text.trim();
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
    <button>Enviar</button>
  </form>
</aside>
