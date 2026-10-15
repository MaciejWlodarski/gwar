#!/usr/bin/env node
/** Real local DS + browser IndexedDB/Web Locks/wasm, no UI. Never reuses a server. */
import { stopProcesses } from "./processes.mjs";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { copyFile, mkdtemp, rm } from "node:fs/promises";
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
  const binary = process.env.VC_SERVER_BIN ?? path.join(root, "target/debug/vc-server");
  const startServer = () => start(binary, ["--data-dir", directory, "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"], root);
  let service = startServer();
  const vite = start("pnpm", ["--filter", "web", "exec", "vite", "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], root);
  await Promise.all([ready(`${web}/`, vite), ready(`http://127.0.0.1:${serverPort}/health`, service)]);
  const until = async (what, check, timeout = 30_000) => {
    for (const started = Date.now(); Date.now() - started < timeout;) {
      if (await check()) return;
      for (const page of pages) await page.evaluate(() => window.mls.runtime.sync().catch(() => {}));
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    for (const page of pages) console.error(await page.evaluate(() => window.mls.errors));
    throw new Error(`timed out: ${what}`);
  };
  const snapshot = path.join(directory, "before-joins.sqlite3");
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  // Block every non-local request, including page-level Connect UI effects.
  await context.route("**/*", route => { const host = new URL(route.request().url()).hostname; return ["127.0.0.1", "localhost"].includes(host) ? route.continue() : route.abort(); });
  const pages = [], devices = [[11, 1], [21, 2], [11, 3], [21, 4]];
  for (const [account, device] of devices) {
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
      // The server's state before the other devices join, to lose it later.
      execFileSync(binary, ["--data-dir", directory, "backup", snapshot]);
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
  // The server loses the joins (restored from the earlier backup): every device
  // is now ahead of it, and after two checks a minute apart one of them
  // restarts the conversation; the others accept the chain and join.
  const conversation = message.conversation;
  /** The server comes back from an earlier backup; every device reconnects to it. */
  const restore = async backup => {
    await Promise.all(pages.map(page => page.evaluate(() => window.mls.runtime.close())));
    await stopProcesses([service]);
    await copyFile(backup, path.join(directory, "vc.sqlite3"));
    for (const suffix of ["-wal", "-shm"]) await rm(path.join(directory, `vc.sqlite3${suffix}`), { force: true });
    service = startServer();
    await ready(`http://127.0.0.1:${serverPort}/health`, service);
    for (const [i, page] of pages.entries()) {
      await page.reload();
      await page.evaluate(async ({ account, device, ws }) => { const { connect } = await import("/e2e/mls-fixture.ts"); window.mls = await connect(account, device, ws); }, { account: devices[i][0], device: devices[i][1], ws });
    }
  };
  await restore(snapshot);
  await until("the conversation is restarted", async () => (await Promise.all(pages.map(page => page.evaluate(c => window.mls.runtime.status(c) === "ready" && window.mls.runtime.roster(c)?.restarted === true, conversation)))).every(Boolean), 150_000);
  const groups = new Set(await Promise.all(pages.map(page => page.evaluate(c => window.mls.runtime.roster(c).group, conversation))));
  assert.equal(groups.size, 1); assert.ok(!groups.has(message.group));
  const restarted = await pages[2].evaluate(c => window.mls.seal(c), conversation);
  await until("the restarted group's message opens", async () => (await Promise.all(pages.map(page => page.evaluate(m => window.mls.runtime.openMessage(m).then(r => r.ok?.text), restarted)))).every(text => text === "encrypted hello"));
  for (const page of pages) assert.equal((await page.evaluate(m => window.mls.runtime.openMessage(m), message)).ok?.text, "encrypted hello");
  console.log("PASS a server behind every device leads to one restart that all devices follow, keeping earlier keys");
  // An encrypted channel: its creator makes the group, another member's device
  // joins it by itself and gets the earlier key through a routed assignment.
  // Printed once, by the server's first start.
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(children.map(child => child.output).join("\n"))?.[1];
  assert.ok(token, "admin token in the server log");
  const channel = await pages[0].evaluate(async token => {
    await window.mls.link.request("token.redeem", { token });
    const channel = await window.mls.link.request("channel.create", { name: "Sealed", encrypted: true, history: true });
    window.mls.runtime.channelCreated(channel.id);
    return channel;
  }, token);
  const sealedChannel = `channel:${channel.id}`;
  await until("the creator's channel group", () => pages[0].evaluate(c => window.mls.runtime.status(c) === "ready", sealedChannel));
  const channelMessage = await pages[0].evaluate(c => window.mls.seal(c), sealedChannel);
  // The channel's server state before anyone else joins, to lose it later.
  const beforeJoin = path.join(directory, "before-channel-join.sqlite3");
  execFileSync(binary, ["--data-dir", directory, "backup", beforeJoin]);
  await until("the other member joins", () => pages[3].evaluate(c => window.mls.runtime.status(c) === "ready", sealedChannel));
  // The first device the server asks may lack the earlier epoch (it joined
  // after the message); the next request is allowed a minute later.
  await until("the earlier key arrives", async () => (await pages[3].evaluate(m => window.mls.runtime.openMessage(m), channelMessage)).ok?.text === "encrypted hello", 90_000);
  console.log("PASS a member joins an encrypted channel's group and receives its earlier key");
  // The server loses that join too: the channel restarts like a direct message,
  // its members follow the one new group and keep the earlier key.
  const members = [pages[0], pages[3]];
  await restore(beforeJoin);
  await until("the channel is restarted", async () => (await Promise.all(members.map(page => page.evaluate(c => window.mls.runtime.status(c) === "ready" && window.mls.runtime.roster(c)?.restarted === true, sealedChannel)))).every(Boolean), 150_000);
  const channelGroups = new Set(await Promise.all(members.map(page => page.evaluate(c => window.mls.runtime.roster(c).group, sealedChannel))));
  assert.equal(channelGroups.size, 1); assert.ok(!channelGroups.has(channelMessage.group));
  const afterRestart = await pages[3].evaluate(c => window.mls.seal(c), sealedChannel);
  await until("the restarted channel's message opens", async () => (await pages[0].evaluate(m => window.mls.runtime.openMessage(m).then(r => r.ok?.text), afterRestart)) === "encrypted hello");
  for (const page of members) assert.equal((await page.evaluate(m => window.mls.runtime.openMessage(m), channelMessage)).ok?.text, "encrypted hello");
  console.log("PASS a server behind an encrypted channel's members leads to one restart they all follow, keeping earlier keys");
  // Deleting the channel deletes its group on every device.
  await pages[0].evaluate(id => window.mls.link.request("channel.delete", { channel: id }), channel.id);
  await until("the group is forgotten", () => pages[3].evaluate(m => window.mls.epochKey(m.group, m.epoch).then(key => key === null), channelMessage));
  console.log("PASS deleting an encrypted channel deletes its group state");
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
  // With takeover enabled, as in the app, the newest tab wins instead of waiting.
  await waiting.evaluate(() => window.mls.runtime.close());
  const older = await context.newPage(), newer = await context.newPage();
  for (const page of [older, newer]) {
    await page.goto(`${web}/e2e/mls.html`);
    await page.evaluate(async ws => { const { connect } = await import("/e2e/mls-fixture.ts"); window.mls = await connect(21, 2, ws, { takeover: true }); }, ws);
  }
  assert.equal(await older.evaluate(() => window.mls.takenOver()), true);
  assert.equal((await newer.evaluate(m => window.mls.runtime.openMessage(m), message)).ok?.text, "encrypted hello");
  pages.push(older, newer);
  console.log("PASS a newer tab of the same device takes the session over");
  await Promise.all(pages.map(page => page.evaluate(() => window.mls.runtime.close())));
  console.log("PASS device restart reloads durable MLS state");

} catch (error) {
  // The local services' output explains most failures (in CI there is nothing else to look at).
  for (const child of children) console.error(`--- ${child.spawnargs.join(" ")}\n${child.output}`);
  throw error;
} finally {
  await browser?.close();
  await stopProcesses(children);
  await rm(directory, { recursive: true, force: true });
}
