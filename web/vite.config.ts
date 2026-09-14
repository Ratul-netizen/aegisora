import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The dev server proxies /api to uops-server rather than the app calling it across an
// origin. Same-origin in development means the session cookie behaves exactly as it
// will in production, where a reverse proxy serves both from one host — and it means no
// CORS configuration exists to be got wrong, or to be left permissive by accident.
//
// The target is fixed rather than read from the environment. A configurable one wants
// node types in this file for `process.env`, and the only reason to point the dev proxy
// somewhere else is to develop the UI against someone else's server, which is not a
// thing this project wants to make easy.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8080",
        changeOrigin: false,
      },
    },
  },
  build: {
    // Served by a reverse proxy alongside the API. No CDN, no separate origin.
    outDir: "dist",
    sourcemap: true,
  },
});
