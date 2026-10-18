#!/usr/bin/env node
/** The key archive (docs/e2e-build.md, section 7): real local DS and Connect, browser wasm, no UI. Never reuses a server. */
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
const directory = await mkdtemp(path.join(tmpdir(), "gwar-archive-browser-"));
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
  const webPort = await port(), serverPort = await port(), connectPort = await port();
  const web = `http://127.0.0.1:${webPort}`, ws = `ws://127.0.0.1:${serverPort}/ws`, connect = `http://127.0.0.1:${connectPort}`;
  const target = path.join(root, "target/debug");
  const service = start(process.env.VC_SERVER_BIN ?? path.join(target, "vc-server"), ["--data-dir", path.join(directory, "server"), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"], root);
  const connectService = start(process.env.GWAR_CONNECT_BIN ?? path.join(target, "gwar-connect"), ["--data-dir", path.join(directory, "connect"), "--http", `127.0.0.1:${connectPort}`, "--origin", web], root);
  const vite = start("pnpm", ["--filter", "web", "exec", "vite", "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], root);
  await Promise.all([ready(`${web}/`, vite), ready(`http://127.0.0.1:${serverPort}/health`, service), ready(`${connect}/v1/service-key`, connectService)]);

  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  await context.route("**/*", route => { const host = new URL(route.request().url()).hostname; return ["127.0.0.1", "localhost"].includes(host) ? route.continue() : route.abort(); });
  const pages = [];
  const open = async () => {
    const page = await context.newPage(); pages.push(page);
    page.on("pageerror", e => console.error("browser:", e));
    await page.goto(`${web}/e2e/mls.html`);
    return page;
  };
  const device = (page, record, options = {}) => page.evaluate(async ({ record, ws, connect, options }) => {
    const fixture = await import("/e2e/archive-fixture.ts");
    window.device = await fixture.connect(record, ws, connect, options);
    return window.device.uid;
  }, { record, ws, connect, options });
  const archive = page => page.evaluate(() => window.device.runtime.archiveNow());
  const until = async (what, check, live, timeout = 30_000) => {
    for (const started = Date.now(); Date.now() - started < timeout;) {
      if (await check()) return;
      for (const page of live) await page.evaluate(() => window.device.runtime.sync().catch(() => {}));
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    for (const page of live) console.error(await page.evaluate(() => window.device.errors));
    throw new Error(`timed out: ${what}`);
  };

  // Two accounts and their first devices; Alice's DM message is backed up.
  const [laptop, bob] = [await open(), await open()];
  const alice = await laptop.evaluate(c => import("/e2e/archive-fixture.ts").then(f => f.account.create(c, "alice", "correct horse battery")), connect);
  const bobAccount = await bob.evaluate(c => import("/e2e/archive-fixture.ts").then(f => f.account.create(c, "bob", "correct horse battery")), connect);
  const conversation = `dm:${[await device(laptop, alice.record), await device(bob, bobAccount.record)].sort().join(":")}`;
  await until("the conversation is created", async () => {
    try { await laptop.evaluate(c => window.device.runtime.openConversation(c), conversation); return true; }
    catch { return false; }
  }, [laptop, bob]);
  await until("Bob is in the group", async () => {
    try { await bob.evaluate(() => window.device.runtime.sync()); return true; } catch { return false; }
  }, [laptop, bob]);
  const message = await laptop.evaluate(c => window.device.seal(c, "before the backup"), conversation);
  assert.equal((await bob.evaluate(m => window.device.runtime.openMessage(m), message)).ok?.text, "before the backup");
  const first = await archive(laptop);
  assert.equal(first.safe, 1);
  assert.equal(first.recovery_version, 1);
  console.log("PASS a device backs up its epoch keys under generation 1, sealed to the recovery key");

  // A second device of Alice, with uploads off, opens generation 1 after the laptop's transfer.
  const phone = await open();
  await device(phone, await phone.evaluate(c => import("/e2e/archive-fixture.ts").then(f => f.account.signIn(c, "alice", "correct horse battery")), connect), { uploads: false });
  assert.deepEqual((await archive(phone)).opened, []);
  assert.equal((await archive(laptop)).waiting.length, 1);
  const phoneState = await archive(phone);
  assert.deepEqual(phoneState.opened, [1]);
  assert.equal(phoneState.newest, 1, "uploads off: the phone made no generation");
  assert.deepEqual((await archive(laptop)).waiting, []);
  console.log("PASS a second device opens the archive after a transfer; the setting off uploads nothing");

  // Every device is lost; the recovery code restores the history on a new one.
  for (const page of pages) await page.evaluate(() => window.device.runtime.close());
  const recovered = await open();
  const record = await recovered.evaluate(({ c, code }) => import("/e2e/archive-fixture.ts").then(f => f.account.recover(c, "alice", code, "another horse battery")), { c: connect, code: alice.recoveryCode });
  await device(recovered, record);
  await until("the recovered device joins the conversation", async () => {
    try { await recovered.evaluate(c => window.device.runtime.openConversation(c), conversation); return true; }
    catch { return false; }
  }, [recovered]);
  const restored = await archive(recovered);
  assert.ok(restored.opened.includes(1));
  assert.equal(await recovered.evaluate(() => window.device.record().archiveRecoverySecret), undefined);
  assert.equal((await recovered.evaluate(m => window.device.runtime.openMessage(m), message)).ok?.text, "before the backup");
  console.log("PASS the recovery code restores the history on a new device");
} catch (error) {
  // The local services' output explains most failures (in CI there is nothing else to look at).
  for (const child of children) console.error(`--- ${child.spawnargs.join(" ")}\n${child.output}`);
  throw error;
} finally {
  await browser?.close();
  await stopProcesses(children);
  await rm(directory, { recursive: true, force: true });
}
