import { defineConfig } from "vite";

// `tauri android dev` / `tauri ios dev` on a real device sets TAURI_DEV_HOST
// to this machine's LAN address so the phone can reach the dev server.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**", "**/plugins/**"] },
  },
  build: {
    target: ["es2021", "safari15", "chrome100"],
    outDir: "dist",
    emptyOutDir: true,
  },
});
