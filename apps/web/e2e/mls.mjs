#!/usr/bin/env node
/** Real local DS + browser IndexedDB/Web Locks/wasm, no UI. Never reuses a server. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const directory = await mkdtemp(path.join(tmpdir(), "gwar-mls-browser-"));
const children = [];
let browser;
async function port() {
  const server = net.createServer(); await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port; await new Promise(resolve => server.close(resolve)); return port;
}
function start(command, args, cwd) {
  const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"] });
  child.output = ""; child.stdout.on("data", b => { child.output += b; }); child.stderr.on("data", b => { child.output += b; });
  children.push(child); return child;
}
async function ready(url, child) {
  for (let i = 0; i < 200; i++) {
    if (child.exitCode !== null) throw new Error(child.output);
    try { if ((await fetch(url)).ok) return; } catch { /* starting */ }
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`local service not ready: ${url}\n${child.output}`);
}
try {
  const webPort = await port(), serverPort = await port();
  const web = `http://127.0.0.1:${webPort}`, ws = `ws://127.0.0.1:${serverPort}/ws`;
  const service = start(process.env.VC_SERVER_BIN ?? path.join(root, "target/debug/vc-server"), ["--data-dir", directory, "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"], root);
  const vite = start("pnpm", ["--filter", "web", "exec", "vite", "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], root);
  await Promise.all([ready(`${web}/`, vite), ready(`http://127.0.0.1:${serverPort}/health`, service)]);
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  // Block every non-local request, including page-level Connect UI effects.
  await context.route("**/*", route => { const host = new URL(route.request().url()).hostname; return ["127.0.0.1", "localhost"].includes(host) ? route.continue() : route.abort(); });
  const pages = [];
  for (const [account, device] of [[11, 1], [21, 2], [11, 3], [21, 4]]) {
    const page = await context.newPage(); pages.push(page);
    page.on("pageerror", e => console.error("browser:", e));
    // Vite serves a blank fixture; the application controller never connects.
    await page.goto(`${web}/e2e/mls.html`);
    await page.evaluate(async ({ account, device, ws }) => {
      const { connect } = await import("/e2e/mls-fixture.ts");
      window.mls = await connect(account, device, ws);
    }, { account, device, ws });
    // Start the additional two devices only after the first epoch is sealed.
    if (pages.length === 2) {
      const uids = await Promise.all(pages.map(p => p.evaluate(() => window.mls.uid)));
      const conversation = `dm:${uids.sort().join(":")}`;
      await pages[0].evaluate(c => window.mls.runtime.openConversation(c), conversation);
      await pages[1].evaluate(() => window.mls.runtime.sync());
      const message = await pages[0].evaluate(c => window.mls.seal(c), conversation);
      const opened = await pages[1].evaluate(m => window.mls.runtime.openMessage(m), message);
      assert.equal(opened.ok?.text, "encrypted hello");
      const keys = await Promise.all(pages.map(p => p.evaluate(m => window.mls.epochKey(m.group, m.epoch), message)));
      assert.ok(keys[0]?.length === 32); assert.deepEqual(keys[0], keys[1]);
      await pages[0].evaluate(m => { window.firstMessage = m; }, message);
      console.log("PASS two users derive identical epoch keys and open a sealed message");
    }

  }
  // Both new devices join externally and receive earlier keys through durable history.
  const message = await pages[0].evaluate(() => window.firstMessage);
  for (let pass = 0; pass < 3; pass++) for (const page of pages) await page.evaluate(() => window.mls.runtime.sync());
  for (const page of pages.slice(2)) {
    const opened = await page.evaluate(m => window.mls.runtime.openMessage(m), message);
    assert.equal(opened.ok?.text, "encrypted hello");
    const key = await page.evaluate(m => window.mls.epochKey(m.group, m.epoch), message);
    assert.deepEqual(key, await pages[0].evaluate(m => window.mls.epochKey(m.group, m.epoch), message));
  }
  console.log("PASS additional devices join and import pre-join message history");
  // Restart on the same origin/device: IndexedDB state and Welcome dedupe survive.
  await pages[1].evaluate(() => window.mls.runtime.close());
  await pages[1].reload();
  await pages[1].evaluate(async ws => { const { connect } = await import("/e2e/mls-fixture.ts"); window.mls = await connect(21, 2, ws); }, ws);
  assert.equal((await pages[1].evaluate(m => window.mls.runtime.openMessage(m), message)).ok?.text, "encrypted hello");
  const waiting = await context.newPage();
  await waiting.goto(`${web}/e2e/mls.html`);
  await waiting.evaluate(async ws => {
    const { connect } = await import("/e2e/mls-fixture.ts");
    window.acquired = false;
    window.waiting = connect(21, 2, ws).then(mls => { window.mls = mls; window.acquired = true; });
  }, ws);
  await new Promise(resolve => setTimeout(resolve, 250));
  assert.equal(await waiting.evaluate(() => window.acquired), false);
  await pages[1].evaluate(() => window.mls.runtime.close());
  await waiting.evaluate(() => window.waiting);
  assert.equal((await waiting.evaluate(m => window.mls.runtime.openMessage(m), message)).ok?.text, "encrypted hello");
  pages.push(waiting);
  console.log("PASS second tab waits for the device lock, then reloads durable state");
  await Promise.all(pages.map(page => page.evaluate(() => window.mls.runtime.close())));
  console.log("PASS device restart reloads durable MLS state");

} finally {
  await browser?.close();
  for (const child of children) child.kill("SIGTERM");
  await Promise.all(children.map(child => child.exitCode !== null ? undefined : new Promise(resolve => child.once("exit", resolve))));
  await rm(directory, { recursive: true, force: true });
}
