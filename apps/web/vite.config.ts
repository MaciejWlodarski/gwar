import { contentSecurityPolicy } from "./src/net/csp.ts";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

// The app's version comes from package.json (set by scripts/bump-version.sh) and the commit it is
// built from (CI's GITHUB_SHA, else git), so reports and the server know exactly which build runs.
const version = (JSON.parse(readFileSync(new URL("./package.json", import.meta.url), "utf8")) as { version: string }).version;
const commit = process.env.GITHUB_SHA ?? (() => {
  try { return execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim(); } catch { return ""; }
})();

// The dev server proxies the WebSocket so `pnpm dev` works against a local
// `gwar-server` without CORS or mixed-content issues.
export default defineConfig(({ command, mode }) => ({
  plugins: [react(), tailwindcss(), {
    name: "production-csp",
    transformIndexHtml(html) {
      // Only the isolated browser fixture permits plaintext loopback services.
      const policy = contentSecurityPolicy(mode === "desktop");
      const e2ePolicy = mode === "e2e" ? policy.replace("connect-src 'self' https: wss:", "connect-src 'self' https: wss: http://127.0.0.1:* http://localhost:* ws://127.0.0.1:* ws://localhost:*") : policy;
      return command === "build" ? html.replace("<head>", `<head>\n    <meta http-equiv="Content-Security-Policy" content="${e2ePolicy}" />`) : html;
    },
  }],
  server: {
    port: 5173,
    proxy: {
      "/ws": { target: "ws://127.0.0.1:8790", ws: true },
    },
  },
  // licenses.md: the bundled packages' licenses, served with the app and shipped in the desktop bundle.
  build: { target: "es2022", sourcemap: true, license: { fileName: "licenses.md" } },
  define: { __APP_VERSION__: JSON.stringify(version), __APP_COMMIT__: JSON.stringify(commit) },
  // The browser e2e fixtures are imported at run time: scanning them up front
  // keeps Vite from reloading the pages when it finds their dependencies.
  optimizeDeps: { entries: ["index.html", "e2e/*.html", "e2e/*-fixture.{ts,tsx}"] },
  test: {
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
}));
