import { contentSecurityPolicy } from "./src/net/csp.ts";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// The dev server proxies the WebSocket so `pnpm dev` works against a local
// `vc-server` without CORS or mixed-content issues.
export default defineConfig(({ command, mode }) => ({
  plugins: [react(), tailwindcss(), {
    name: "production-csp",
    transformIndexHtml(html) {
      return command === "build" ? html.replace("<head>", `<head>\n    <meta http-equiv="Content-Security-Policy" content="${contentSecurityPolicy(mode === "desktop")}" />`) : html;
    },
  }],
  server: {
    port: 5173,
    proxy: {
      "/ws": { target: "ws://127.0.0.1:8790", ws: true },
    },
  },
  build: { target: "es2022", sourcemap: true },
  test: {
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
}));
