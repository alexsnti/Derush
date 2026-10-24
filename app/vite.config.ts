import { defineConfig } from "vite";

// Port fixe attendu par tauri.conf.json.
export default defineConfig({
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "safari15", outDir: "dist" },
});
