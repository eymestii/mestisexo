import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// En desarrollo, `npm run dev` sirve la interfaz con recarga en caliente y
// reenvía al backend lo que en producción reparte Caddy. Por defecto apunta a
// un repetidor y un servicio de cuentas locales; MESTISEXO_BACKEND lo cambia.
const backend = process.env.MESTISEXO_BACKEND ?? "http://127.0.0.1";
const proxy = (port: number) => (backend.startsWith("https://") ? backend : `${backend}:${port}`);

export default defineConfig({
  plugins: [svelte()],
  // Tauri muestra sus propios errores de compilación; que Vite no borre la consola.
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      "/ws": { target: proxy(8090), ws: true, changeOrigin: true },
      "/hls": { target: proxy(8094), changeOrigin: true },
      "/api": { target: proxy(8092), changeOrigin: true },
      "/login": { target: proxy(8092), changeOrigin: true },
    },
  },
  build: {
    // iOS 17.1 es el mínimo que ya exigía la web para reproducir.
    target: ["es2022", "safari17"],
    outDir: "dist",
    emptyOutDir: true,
    // hls.js ocupa ~180 KB comprimido y no se puede partir; va en su propio
    // trozo, que Safari nunca descarga.
    chunkSizeWarningLimit: 700,
    // Las licencias de lo que va empaquetado (hls.js es Apache-2.0).
    license: { fileName: "LICENSES.md" },
  },
});
