/** Checks the actual production meta policy, including hash-wasm, on a free local port. */
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "../dist");
const require = createRequire(import.meta.url);
const wasmModule = path.join(path.dirname(require.resolve("hash-wasm")), "index.esm.js");
const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".woff2": "font/woff2" };
const server = createServer(async (request, response) => {
  response.setHeader("Content-Security-Policy", "frame-ancestors 'none'");
  response.setHeader("X-Content-Type-Options", "nosniff");
  response.setHeader("Referrer-Policy", "no-referrer");
  try {
    const url = new URL(request.url, "http://localhost");
    const file = url.pathname === "/hash-wasm.js" ? wasmModule : path.resolve(root, `.${url.pathname === "/" ? "/index.html" : url.pathname}`);
    if (file !== wasmModule && !file.startsWith(`${root}/`)) throw new Error("bad path");
    response.setHeader("Content-Type", types[path.extname(file)] ?? "application/octet-stream");
    if (url.pathname === "/probe.js") {
      response.setHeader("Content-Type", "text/javascript");
      response.end("import { argon2id } from '/hash-wasm.js'; window.__cspHash = await argon2id({password:'test',salt:'12345678',parallelism:1,iterations:1,memorySize:1024,hashLength:32,outputType:'hex'});");
    } else response.end(await readFile(file));
  } catch { response.writeHead(404); response.end(); }
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const browser = await chromium.launch({ headless: true });
let passed = 0;
const check = (name, fn) => { fn(); passed++; console.log(`PASS ${name}`); };
try {
  const page = await browser.newPage();
  await page.route("https://**", (route) => route.abort());
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  await page.locator("#root").waitFor();
  await page.waitForFunction(() => document.querySelector("#root")?.childElementCount > 0);
  const initial = await page.evaluate(() => ({ policy: document.querySelector('meta[http-equiv="Content-Security-Policy"]')?.content, theme: document.documentElement.dataset.theme }));
  check("production policy and static theme bootstrap", () => { assert.match(initial.policy, /script-src 'self' 'wasm-unsafe-eval'/); assert.match(initial.theme, /^(dark|light)$/); });
  await page.evaluate(() => {
    window.__violations = [];
    document.addEventListener("securitypolicyviolation", (event) => window.__violations.push(event.effectiveDirective));
    const script = document.createElement("script"); script.textContent = "window.__inlineRan = true"; document.head.append(script);
    const base = document.createElement("base"); base.href = "https://hostile.example/"; document.head.append(base);
    const probe = document.createElement("script"); probe.type = "module"; probe.src = "/probe.js"; document.head.append(probe);
  });
  await page.waitForFunction(() => typeof window.__cspHash === "string");
  const state = await page.evaluate(() => ({ inline: window.__inlineRan, violations: window.__violations, hash: window.__cspHash, base: document.baseURI, origin: location.origin }));
  check("inline script blocked", () => { assert.equal(state.inline, undefined); assert(state.violations.includes("script-src-elem")); });
  check("hostile base URI blocked", () => { assert.equal(state.base, `${state.origin}/`); assert(state.violations.includes("base-uri")); });
  check("real hash-wasm Argon2 works under CSP", () => assert.match(state.hash, /^[a-f0-9]{64}$/));
  const blocked = await page.evaluate(async () => { try { await fetch("http://127.0.0.1:1/x"); return false; } catch { return true; } });
  check("production HTTP connection blocked", () => assert.equal(blocked, true));
  console.log(`${passed} passed, 0 failed`);
} finally {
  await browser.close();
  server.closeAllConnections();
  await new Promise((resolve) => server.close(resolve));
}
