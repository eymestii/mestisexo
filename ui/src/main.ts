import { mount } from "svelte";
import "./app.css";
import { native } from "./lib/tauri";

// La misma interfaz sirve para la app del streamer (Tauri) y para la web de
// los espectadores. Cada una va en su propio trozo: el espectador no descarga
// el código del streamer, ni el streamer el reproductor.
const target = document.body;
if (native) {
  import("./host/Host.svelte").then(({ default: Host }) => mount(Host, { target }));
} else {
  import("./components/Viewer.svelte").then(({ default: Viewer }) => mount(Viewer, { target }));
}
