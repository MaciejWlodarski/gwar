#!/usr/bin/env node
/** Encrypted private channels (docs/e2e-build.md, section 6, 6c) with a real local server and
 * browser runtimes, no UI: the first admin becomes the owner, signs the first manager pair,
 * creates a channel admitting one member; the member reads it, an outsider never does, and a
 * new member list removes the member. Never reuses a server. */
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
const directory = await mkdtemp(path.join(tmpdir(), "gwar-private-browser-"));
const children = [];
let browser;
async function port() {
  const server = net.createServer(); await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const port = server.address().port; await new Promise(resolve => server.close(resolve)); return port;
}
function start(command, args, cwd) {
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
  const binary = process.env.VC_SERVER_BIN ?? path.join(root, "target/debug/vc-server");
  const service = start(binary, ["--data-dir", directory, "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"], root);
  const vite = start("pnpm", ["--filter", "web", "exec", "vite", "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], root);
  await Promise.all([ready(`${web}/`, vite), ready(`http://127.0.0.1:${serverPort}/health`, service)]);
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(service.output)?.[1];
  assert.ok(token, "the server prints its first admin token");
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  await context.route("**/*", route => { const host = new URL(route.request().url()).hostname; return ["127.0.0.1", "localhost"].includes(host) ? route.continue() : route.abort(); });
  const pages = [];
  const until = async (what, check, timeout = 30_000) => {
    for (const started = Date.now(); Date.now() - started < timeout;) {
      if (await check()) return;
      for (const page of pages) await page.evaluate(() => window.mls.runtime.sync().catch(() => {}));
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    for (const page of pages) console.error(await page.evaluate(() => window.mls.errors));
    throw new Error(`timed out: ${what}`);
  };
  // The owner (account 11), a member (21) and an outsider (31), one device each.
  for (const [account, device] of [[11, 1], [21, 2], [31, 3]]) {
    const page = await context.newPage(); pages.push(page);
    page.on("pageerror", e => console.error("browser:", e));
    await page.goto(`${web}/e2e/mls.html`);
    await page.evaluate(async ({ account, device, ws }) => {
      const { connect } = await import("/e2e/mls-fixture.ts");
      window.mls = await connect(account, device, ws);
    }, { account, device, ws });
  }
  const [owner, member, outsider] = pages;
  const memberUid = await member.evaluate(() => window.mls.uid);

  // The first admin token makes its redeemer the owner; every device pins that owner.
  await owner.evaluate(t => window.mls.link.request("token.redeem", { token: t }), token);
  await until("every device pins the owner", async () => (await Promise.all(pages.map(p => p.evaluate(() => window.mls.runtime.ownerView()?.owner)))).every(Boolean));
  // The owner signs the first anchor (no confirmers) with its account key and a list under it.
  await owner.evaluate(async () => {
    const { anchorEnvelope } = await import("/src/mls/wasm.ts");
    const statement = await window.mls.runtime.nextAnchor([]);
    const anchor = await anchorEnvelope(statement, window.mls.accountKey, window.mls.signAccount(statement));
    await window.mls.runtime.publishManagers([], anchor);
  });
  await until("every device accepts the manager pair", async () => (await Promise.all(pages.map(p => p.evaluate(() => window.mls.runtime.ownerView()?.anchor_seq)))).every(seq => seq === 1));
  console.log("PASS the first admin is the owner and signs the first manager pair");

  // An encrypted private channel admitting the member: its group carries the signed lists.
  const channel = await owner.evaluate(async uid => {
    const c = await window.mls.link.request("channel.create", { name: "sealed", private: true, encrypted: true, history: true });
    window.mls.runtime.channelCreated(c.id, [uid]);
    return c.id;
  }, memberUid);
  const conversation = `channel:${channel}`;
  await until("the owner's group is ready", async () => (await owner.evaluate(c => window.mls.runtime.status(c), conversation)) === "ready");
  await until("the member joins", async () => (await member.evaluate(c => window.mls.runtime.status(c), conversation)) === "ready");
  const message = await owner.evaluate(c => window.mls.seal(c), conversation);
  assert.equal((await member.evaluate(m => window.mls.runtime.openMessage(m), message)).ok?.text, "encrypted hello");
  console.log("PASS a member admitted by the signed list reads the encrypted private channel");
  assert.notEqual(await outsider.evaluate(c => window.mls.runtime.status(c), conversation), "ready");
  assert.equal((await outsider.evaluate(m => window.mls.runtime.openMessage(m), message)).ok, undefined);
  console.log("PASS an outsider holds no group and cannot read it");

  // A new member list without the member removes its device in the same commit.
  await owner.evaluate(id => window.mls.runtime.setPrivateMembers(id, []), channel);
  await until("the member loses the channel", async () => (await member.evaluate(c => window.mls.runtime.status(c), conversation)) !== "ready");
  const after = await owner.evaluate(c => window.mls.seal(c), conversation);
  assert.equal((await member.evaluate(m => window.mls.runtime.openMessage(m), after)).ok, undefined);
  console.log("PASS a new member list removes the member, who reads nothing after it");
  await Promise.all(pages.map(page => page.evaluate(() => window.mls.runtime.close())));
} catch (error) {
  for (const child of children) console.error(`--- ${child.spawnargs.join(" ")}\n${child.output}`);
  throw error;
} finally {
  await browser?.close();
  await stopProcesses(children);
  await rm(directory, { recursive: true, force: true });
}
