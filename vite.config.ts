import { defineConfig } from "vite";

export default defineConfig({
  // Tauri drives the dev server on a fixed port and needs a hard failure if it is taken.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**", "**/target/**"] },
  },
  build: { target: "es2021", minify: "esbuild", sourcemap: false },
});
