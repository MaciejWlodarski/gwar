#!/usr/bin/env node
/** Real local DS + encrypted DM controller and React UI. Never reuses a server. */
import { stopProcesses } from "./processes.mjs";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import net from "node:net";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const directory = await mkdtemp(path.join(tmpdir(), "gwar-dm-browser-"));
const children = [];
let browser;
async function port() {
  const server = net.createServer(); await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port; await new Promise(resolve => server.close(resolve)); return port;
}
function start(command, args, cwd) {
  // Own process group: `pnpm exec` does not reliably pass SIGTERM on to vite.
  const child = spawn(command, args, { cwd, stdio: ["ignore", "pipe", "pipe"], detached: true });
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
  const contexts = [], pages = [];
  async function device(account, device) {
    const context = await browser.newContext(); contexts.push(context);
    await context.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
    const page = await context.newPage(); pages.push(page);
    page.on("pageerror", e => console.error("browser:", e));
    await page.goto(`${web}/e2e/dm.html`);
    await connect(page, account, device);
    return page;
  }
  async function connect(page, account, device) {
    await page.evaluate(async ({ account, device, ws }) => { const fixture = await import("/e2e/dm-fixture.tsx"); window.dm = await fixture.connect(account, device, ws); }, { account, device, ws });
  }
  async function open(page, peer) {
    await page.evaluate(peer => window.dm.open(peer), peer);
    await page.waitForFunction(() => { const el = document.querySelector('textarea[aria-label="Write a message"]') ?? document.querySelector("textarea"); return el && !el.disabled; }, null, { timeout: 45000 });
  }
  async function send(page, text) {
    const input = page.locator("textarea").first();
    await input.fill(text); await input.press("Enter");
    await page.getByText(text, { exact: true }).waitFor({ timeout: 45000 });
  }
  async function sync(...active) {
    for (let i = 0; i < 3; i++) for (const page of active) await page.evaluate(() => window.dm.sync());
  }
  const a = await device(11, 1), b = await device(21, 2);
  const auid = await a.evaluate(() => window.dm.uid), buid = await b.evaluate(() => window.dm.uid);
  await b.evaluate(() => window.dm.close());
  await open(a, buid);
  await send(a, "offline encrypted hello");
  console.log("PASS encrypted composer sends to an offline member");
  await connect(b, 21, 2);
  assert.equal((await b.evaluate(peer => window.dm.debug().threads[`dm:${peer}`]?.unread, auid)), 1);
  await open(b, auid); await sync(a, b);
  await b.getByText("offline encrypted hello", { exact: true }).waitFor({ timeout: 45000 });
  const original = await a.evaluate(peer => window.dm.rows(peer).find(i => i.sealed).sealed, buid);
  assert.ok(!original.ciphertext.includes("offline encrypted hello"));
  console.log("PASS offline recipient connects and opens stored ciphertext");
  // UI edit and own deletion use sealed revisions, not plaintext channel endpoints.
  await a.getByRole("button", { name: "Edit message", exact: true }).click();
  await a.getByRole("textbox", { name: "Edit message" }).fill("edited encrypted hello");
  await a.getByRole("button", { name: "Save", exact: true }).click();
  await b.getByText("edited encrypted hello", { exact: false }).waitFor({ timeout: 45000 });
  await a.getByRole("button", { name: "Delete message", exact: true }).click({ modifiers: ["Shift"] });
  await b.waitForFunction(peer => window.dm.rows(peer).some(i => i.sealed?.deleted && i.authentic), auid);
  assert.equal(await b.getByText("edited encrypted hello", { exact: false }).count(), 0);
  console.log("PASS encrypted edit and authenticated own deletion");
  await send(a, "history for a second device");
  const a2 = await device(11, 3); await open(a2, buid); await sync(a, b, a2);
  await a2.getByText("history for a second device", { exact: true }).waitFor({ timeout: 45000 });
  console.log("PASS second sender device receives history");
  await Promise.all([a, b, a2].map(p => p.evaluate(() => window.dm.close())));
  const b2 = await device(21, 4); await open(b2, auid);
  await b2.getByText("Can't open this message on this device", { exact: true }).first().waitFor({ timeout: 45000 });
  await b2.getByText("Earlier messages will appear when another member is online.", { exact: true }).waitFor();
  await b2.getByText("Deleted", { exact: true }).waitFor();
  console.log("PASS missing keys and unopened deletion have muted UI states");
  await connect(a, 11, 1); await open(a, buid); await sync(a, b2);
  await b2.getByText("history for a second device", { exact: true }).waitFor({ timeout: 45000 });
  console.log("PASS second recipient device re-renders when history keys arrive");
  // Every remaining device learns the signed revocation; the disposable DS runs --no-connect.
  await a.evaluate(() => window.dm.revoke(21, 2));
  await b2.evaluate(() => window.dm.revoke(21, 2));
  await sync(a, b2);
  await send(a, "after revocation");
  await b2.getByText("after revocation", { exact: true }).waitFor({ timeout: 45000 });
  const newest = await a.evaluate(peer => window.dm.rows(peer).find(i => i.msg?.text === "after revocation").sealed, buid);
  await connect(b, 21, 2);
  await b.evaluate(() => window.dm.revoke(21, 2).catch(() => {}));
  await sync(a, b2);
  const result = await b.evaluate(row => window.dm.openRaw(row), newest);
  assert.equal(result.reason, "no_key");
  console.log("PASS revoked device has no key for new messages while remaining device reads them");
  await Promise.all([a, b2].map(page => page.evaluate(async () => {
    const { useSession } = await import("/src/state/stores.ts");
    useSession.getState().dispatch({ type: "selectChannel", channel: 1 });
  })));
  await send(a, "channel chat still works");
  await b2.getByText("channel chat still works", { exact: true }).waitFor();
  console.log("PASS plaintext channel chat still works beside encrypted DMs");
  // A newer tab of the same device takes the session over, as a new window would on the server.
  const tab = await contexts[0].newPage(); pages.push(tab);
  tab.on("pageerror", e => console.error("browser:", e));
  await tab.goto(`${web}/e2e/dm.html`);
  await connect(tab, 11, 1);
  await a.waitForFunction(() => window.dm.debug().closeReason?.reason?.kind === "replaced", null, { timeout: 45000 });
  assert.equal(await tab.evaluate(() => window.dm.debug().phase), "online");
  console.log("PASS a newer tab of the device takes the session over");
  // Without the MLS module (blocked here), the server connection still works; DMs stay securing.
  const blocked = await browser.newContext(); contexts.push(blocked);
  await blocked.route("**/*", route => {
    const url = new URL(route.request().url());
    return !["127.0.0.1", "localhost"].includes(url.hostname) || url.pathname.endsWith(".wasm") ? route.abort() : route.continue();
  });
  const plain = await blocked.newPage(); pages.push(plain);
  await plain.goto(`${web}/e2e/dm.html`);
  await connect(plain, 31, 5);
  assert.deepEqual(await plain.evaluate(() => { const d = window.dm.debug(); return [d.phase, d.mls]; }), ["online", false]);
  await plain.evaluate(peer => window.dm.open(peer), auid);
  await plain.getByPlaceholder("Securing this conversation…").waitFor({ timeout: 45000 });
  console.log("PASS the server connection works when MLS cannot start");
  await Promise.all(pages.map(page => page.evaluate(() => window.dm.close())));

} finally {
  await browser?.close();
  await stopProcesses(children);
  await rm(directory, { recursive: true, force: true });
}
