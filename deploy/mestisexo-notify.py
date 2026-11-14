#!/usr/bin/env python3
"""Avisa en Discord cuando empieza y cuando termina el directo.

Escucha los avisos del repetidor (Server-Sent Events en /events) y los publica
con un webhook de Discord. No pregunta cada poco: la conexión queda abierta y
el repetidor manda algo solo cuando cambia el estado, más un keep-alive cada
15 s. Solo usa la biblioteca estándar de Python.

Entorno:
  EVENTS_TOKEN         el mismo que el repetidor (/etc/mestisexo/events.env)
  DISCORD_WEBHOOK_URL  webhook del canal (/etc/mestisexo/notify.env)
  STREAM_URL           enlace que va en el aviso de inicio
  EVENTS_URL           por defecto http://127.0.0.1:8095/events
  STATE_DIRECTORY      lo pone systemd; ahí se guarda qué se anunció ya
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

EVENTS_URL = os.environ.get("EVENTS_URL", "http://127.0.0.1:8095/events")
TOKEN = os.environ.get("EVENTS_TOKEN", "").strip()
WEBHOOK = os.environ.get("DISCORD_WEBHOOK_URL", "").strip()
STREAM_URL = os.environ.get("STREAM_URL", "").strip()
STATE = os.path.join(os.environ.get("STATE_DIRECTORY", "."), "notify.json")

# Tres keep-alives perdidos: la conexión está muerta aunque el socket no lo sepa.
READ_TIMEOUT = 50
# Lo que se insiste con Discord antes de dar un aviso por perdido.
POST_DEADLINE = 15 * 60


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def load_state():
    try:
        with open(STATE) as f:
            s = json.load(f)
        return {"since": int(s.get("since", 0)), "live": bool(s.get("live", False))}
    except (OSError, ValueError):
        return {"since": 0, "live": False}


def save_state(s):
    tmp = STATE + ".tmp"
    with open(tmp, "w") as f:
        json.dump(s, f)
    os.replace(tmp, STATE)


def duration(seconds):
    h, m = divmod(max(0, seconds) // 60, 60)
    return f"{h} h {m} min" if h else f"{m} min"


def post(content):
    """Publica en el webhook. Reintenta ante fallos de red, 5xx y 429."""
    # Solo se permite @everyone, que va en los textos de los avisos.
    body = json.dumps({"content": content, "allowed_mentions": {"parse": ["everyone"]}}).encode()
    deadline = time.monotonic() + POST_DEADLINE
    wait = 2
    while True:
        req = urllib.request.Request(
            WEBHOOK, data=body, method="POST",
            headers={"Content-Type": "application/json", "User-Agent": "mestisexo-notify"},
        )
        try:
            with urllib.request.urlopen(req, timeout=15):
                return True
        except urllib.error.HTTPError as e:
            if e.code == 429:
                try:
                    wait = max(wait, float(json.load(e).get("retry_after", wait)))
                except ValueError:
                    pass
            elif e.code < 500:
                # 4xx: webhook borrado o mal copiado. Reintentar no lo arregla.
                log(f"Discord rechazó el aviso ({e.code}); revisa DISCORD_WEBHOOK_URL")
                return False
            log(f"Discord respondió {e.code}; reintento en {wait:.0f} s")
        except (urllib.error.URLError, OSError) as e:
            log(f"No se pudo contactar con Discord ({getattr(e, 'reason', e)}); reintento en {wait:.0f} s")
        if time.monotonic() + wait > deadline:
            log("Aviso descartado: Discord no respondió a tiempo")
            return False
        time.sleep(wait)
        wait = min(wait * 2, 120)


def handle(ev, state):
    """Decide qué anunciar. `since` identifica cada directo, así que reconectar
    o reiniciar este proceso no repite avisos."""
    since, live = int(ev.get("since", 0)), bool(ev.get("live"))
    if live and (since != state["since"] or not state["live"]):
        link = f"\n{STREAM_URL}" if STREAM_URL else ""
        if post(f"🔴 **Empezó el directo** @everyone{link}"):
            log(f"Aviso de inicio publicado ({since})")
        state.update(since=since, live=True)
        save_state(state)
    elif not live and since and since == state["since"] and state["live"]:
        if post(f"⚫ **Terminó el directo** @everyone (duró {duration(int(ev.get('duration_s', 0)))})"):
            log(f"Aviso de fin publicado ({since})")
        state.update(live=False)
        save_state(state)
    elif not live and state["live"] and since != state["since"]:
        # El directo anunciado terminó mientras este proceso no estaba: se
        # marca sin publicar, porque no se sabe cuánto duró.
        state.update(since=since, live=False)
        save_state(state)


def listen(state):
    req = urllib.request.Request(EVENTS_URL, headers={"Authorization": f"Bearer {TOKEN}", "Accept": "text/event-stream"})
    with urllib.request.urlopen(req, timeout=READ_TIMEOUT) as r:
        log("Conectado a los avisos del repetidor")
        data = []
        for raw in r:
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if line.startswith("data:"):
                data.append(line[5:].lstrip())
            elif not line and data:
                try:
                    handle(json.loads("\n".join(data)), state)
                except ValueError:
                    log("Aviso ilegible, se ignora")
                data = []


def main():
    if len(TOKEN) < 32 or not WEBHOOK.startswith("https://"):
        log("Faltan EVENTS_TOKEN (32+ caracteres) o DISCORD_WEBHOOK_URL (https://…)")
        sys.exit(2)
    state = load_state()
    wait = 1
    while True:
        started = time.monotonic()
        try:
            listen(state)
            log("El repetidor cerró la conexión")
        except urllib.error.HTTPError as e:
            log(f"El repetidor respondió {e.code}" + (" (EVENTS_TOKEN no coincide)" if e.code == 401 else ""))
        except (urllib.error.URLError, OSError) as e:
            log(f"Sin conexión con el repetidor ({getattr(e, 'reason', e)})")
        # Si la conexión duró, el fallo es nuevo: se reintenta enseguida.
        wait = 1 if time.monotonic() - started > 60 else min(wait * 2, 60)
        time.sleep(wait)


if __name__ == "__main__":
    main()
