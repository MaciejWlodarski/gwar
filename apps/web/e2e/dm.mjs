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
  const service = start(process.env.GWAR_SERVER_BIN ?? path.join(root, "target/debug/gwar-server"), ["--data-dir", directory, "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"], root);
  const vite = start("pnpm", ["--filter", "web", "exec", "vite", "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], root);
  await Promise.all([ready(`${web}/`, vite), ready(`http://127.0.0.1:${serverPort}/health`, service)]);
  browser = await chromium.launch({ headless: true });
  const contexts = [], pages = [];
  async function device(account, device, address = ws) {
    const context = await browser.newContext(); contexts.push(context);
    await context.route("**/*", route => {
      const url = new URL(route.request().url());
      if (address !== ws && url.pathname.endsWith("/v1/revocations")) {
        return route.fulfill({ json: { revocations: [] }, headers: { "access-control-allow-origin": "*" } });
      }
      return ["127.0.0.1", "localhost"].includes(url.hostname) ? route.continue() : route.abort();
    });
    const page = await context.newPage(); pages.push(page);
    page.on("pageerror", e => console.error("browser:", e));
    await page.goto(`${web}/e2e/dm.html`);
    await connect(page, account, device, address);
    return page;
  }
  async function connect(page, account, device, address = ws) {
    await page.evaluate(async ({ account, device, ws }) => { const fixture = await import("/e2e/dm-fixture.tsx"); window.dm = await fixture.connect(account, device, ws); }, { account, device, ws: address });
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
  async function composer(page) {
    await page.waitForFunction(() => { const el = document.querySelector("textarea"); return el && !el.disabled; }, null, { timeout: 45000 });
  }
  /** Through the create dialog: encryption starts off, then the last choice for this server. */
  async function createChannel(page, name, { remembered, history }) {
    await page.evaluate(() => window.dm.openCreate());
    const dialog = page.getByRole("dialog");
    await dialog.getByRole("textbox", { name: "Name" }).fill(name);
    const encrypted = dialog.getByRole("switch", { name: /End-to-end encrypted/ });
    assert.equal(await encrypted.getAttribute("aria-checked"), String(remembered));
    if (!remembered) await encrypted.click();
    // An encrypted channel has no password.
    assert.equal(await dialog.locator('input[type="password"]').count(), 0);
    const shared = dialog.getByRole("switch", { name: /New members see earlier messages/ });
    assert.equal(await shared.getAttribute("aria-checked"), "true");
    if (!history) await shared.click();
    await dialog.getByRole("button", { name: "Create", exact: true }).click();
    await dialog.waitFor({ state: "detached" });
    const id = await (await page.waitForFunction(name => window.dm.channelId(name), name)).jsonValue();
    assert.deepEqual(await page.evaluate(id => window.dm.options(id), id), { encrypted: true, history });
    return id;
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
  assert.ok(!Buffer.from(original.ciphertext, "base64url").includes(Buffer.from("offline encrypted hello")));
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
  // A picture in the encrypted conversation: the server stores and serves only ciphertext.
  const dot = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==", "base64");
  await a.locator('input[type="file"]').setInputFiles({ name: "dot.png", mimeType: "image/png", buffer: dot });
  // Uploaded: the pending file shows its size instead of its progress.
  await a.getByText(`${dot.length} B`, { exact: true }).waitFor({ timeout: 45000 });
  await send(a, "a picture");
  // Without Connect here, revocation status is unknown, so loading waits for a click; it still decrypts and checks.
  await b.getByRole("button", { name: "Load unverified attachment", exact: true }).click({ timeout: 45000 });
  const picture = b.getByRole("button", { name: "Open dot.png", exact: true }).locator("img");
  await picture.waitFor({ timeout: 45000 });
  assert.match(await picture.getAttribute("src"), /^blob:/);
  await b.waitForFunction(() => { const img = document.querySelector('button[aria-label="Open dot.png"] img'); return img?.complete && img.naturalWidth === 1; }, null, { timeout: 45000 });
  const [attached] = await b.evaluate(peer => window.dm.rows(peer).find(i => i.msg?.text === "a picture").msg.attachments, auid);
  assert.equal(attached.url, `/files/${attached.id}/file`);
  assert.equal(attached.sealed.size, dot.length + 28);
  const stored = await fetch(`http://127.0.0.1:${serverPort}${attached.url}`);
  assert.equal(stored.status, 200);
  const ciphertext = Buffer.from(await stored.arrayBuffer());
  assert.equal(ciphertext.length, dot.length + 28);
  assert.ok(!ciphertext.includes(dot.subarray(0, 16)));
  assert.notEqual(stored.headers.get("content-type"), "image/png");
  console.log("PASS an encrypted attachment opened on the other device");
  const pictureRow = a.locator(`[data-message="${await a.evaluate(peer => window.dm.rows(peer).find(i => i.msg?.text === "a picture").msg.id, buid)}"]`);
  await pictureRow.hover();
  await pictureRow.getByRole("button", { name: "Delete message", exact: true }).click({ modifiers: ["Shift"] });
  await b.getByRole("button", { name: "Open dot.png", exact: true }).waitFor({ state: "detached", timeout: 45000 });
  assert.equal((await fetch(`http://127.0.0.1:${serverPort}${attached.url}`)).status, 404);
  console.log("PASS deleting an encrypted message deletes its file");
  await send(a, "history for a second device");
  const a2 = await device(11, 3); await open(a2, buid); await sync(a, b, a2);
  await a2.getByText("history for a second device", { exact: true }).waitFor({ timeout: 45000 });
  console.log("PASS second sender device receives history");
  await Promise.all([a, b, a2].map(p => p.evaluate(() => window.dm.close())));
  const b2 = await device(21, 4); await open(b2, auid);
  await b2.getByText("Can't open this message on this device", { exact: true }).first().waitFor({ timeout: 45000 });
  await b2.getByText("Earlier messages will appear when another member is online.", { exact: true }).waitFor();
  await b2.getByText("Deleted", { exact: true }).first().waitFor();
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
  for (const page of [a, b2]) await page.evaluate(() => window.dm.selectServer());
  await send(a, "plaintext server hello");
  await b2.getByText("plaintext server hello", { exact: true }).waitFor();
  const plainServerRow = await b2.evaluate(() => window.dm.server().items.find(i => i.msg?.text === "plaintext server hello"));
  assert.equal(plainServerRow.msg.target, "server");
  assert.equal(plainServerRow.msg.signature.group_id, "-");
  assert.equal(plainServerRow.sealed, undefined);
  assert.equal(await b2.getByRole("button", { name: "Who can read", exact: true }).count(), 0);
  console.log("PASS default server chat remains signed plaintext and live-only");
  // Encrypted channels: the creator's group, a member joining it, mentions counted after opening, moderator removal.
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(service.output)?.[1];
  assert.ok(token, "admin token in the server log");
  await a.evaluate(token => window.dm.redeem(token), token);
  const secret = await createChannel(a, "secret", { remembered: false, history: true });
  for (const page of [a, b2]) {
    await page.evaluate(id => window.dm.select(id), secret);
    await composer(page);
  }
  await send(a, "encrypted channel hello");
  await b2.getByText("encrypted channel hello", { exact: true }).waitFor({ timeout: 45000 });
  const sealedRow = await a.evaluate(id => window.dm.channel(id).items.find(i => i.msg?.text === "encrypted channel hello").sealed, secret);
  assert.equal(sealedRow.conversation, `channel:${secret}`);
  assert.ok(!Buffer.from(sealedRow.ciphertext, "base64url").includes(Buffer.from("encrypted channel hello")));
  await a.getByRole("button", { name: "Who can read", exact: true }).click();
  const readers = a.getByRole("dialog");
  await readers.getByText("User21", { exact: true }).waitFor();
  await readers.getByRole("button", { name: "Close", exact: true }).click();
  await readers.waitFor({ state: "detached" });
  console.log("PASS encrypted channel: created, joined, sealed, readers listed");
  await b2.evaluate(() => window.dm.select(1));
  await a.evaluate(peer => window.dm.mention("for you", peer), buid);
  await b2.waitForFunction(id => window.dm.channel(id)?.mentions === 1, secret, { timeout: 45000 });
  console.log("PASS a mention in an encrypted channel counts after opening");
  await b2.evaluate(id => window.dm.select(id), secret);
  await send(b2, "moderate me");
  await a.getByText("moderate me", { exact: true }).waitFor({ timeout: 45000 });
  const moderated = await a.evaluate(id => window.dm.channel(id).items.find(i => i.msg?.text === "moderate me").msg.id, secret);
  const row = a.locator(`[data-message="${moderated}"]`);
  await row.hover();
  await row.getByRole("button", { name: "Delete message", exact: true }).click({ modifiers: ["Shift"] });
  await b2.waitForFunction(([id, msg]) => !window.dm.channel(id).items.some(i => i.msg?.id === msg), [secret, moderated], { timeout: 45000 });
  console.log("PASS a moderator removes a sealed channel message");
  // A new account: earlier messages where the channel shares history, one quiet line where it does not.
  const quiet = await createChannel(a, "quiet", { remembered: true, history: false });
  await a.evaluate(id => window.dm.select(id), quiet);
  await composer(a);
  await send(a, "before you joined");
  const c = await device(41, 7);
  await c.evaluate(() => window.dm.selectServer());
  assert.equal(await c.getByText("plaintext server hello", { exact: true }).count(), 0);
  await c.evaluate(id => window.dm.select(id), quiet);
  await c.getByText("Earlier messages aren't shared with new members.", { exact: true }).waitFor({ timeout: 45000 });
  assert.equal(await c.getByText("Can't open this message on this device", { exact: true }).count(), 0);
  await c.evaluate(id => window.dm.select(id), secret);
  await sync(a, c);
  // History is asked for after joining; the first device asked may be one without the epoch.
  await c.getByText("encrypted channel hello", { exact: true }).waitFor({ timeout: 90000 });
  assert.equal(await c.getByText("Earlier messages aren't shared with new members.", { exact: true }).count(), 0);
  console.log("PASS a new member sees earlier messages only where the channel shares history");
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
  // A separate new database opts into encrypted server chat; all devices join on connect.
  const encryptedPort = await port(), encryptedWs = `ws://127.0.0.1:${encryptedPort}/ws`;
  const encryptedService = start(process.env.GWAR_SERVER_BIN ?? path.join(root, "target/debug/gwar-server"), ["--data-dir", path.join(directory, "encrypted"), "--http", `127.0.0.1:${encryptedPort}`, "--media", `127.0.0.1:${encryptedPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", encryptedWs, "--web-origin", web, "--no-connect", "--encrypted-server-chat"], root);
  await ready(`http://127.0.0.1:${encryptedPort}/health`, encryptedService);
  const d = await device(61, 10, encryptedWs), e = await device(71, 11, encryptedWs);
  // Neither device has opened server chat yet.
  for (const page of [d, e]) await page.waitForFunction(() => window.dm.serverReady(), null, { timeout: 45000 });
  for (const page of [d, e]) { await page.evaluate(() => window.dm.selectServer()); await composer(page); }
  await send(d, "encrypted server hello");
  await e.getByText("encrypted server hello", { exact: true }).waitFor({ timeout: 45000 });
  const serverRow = await d.evaluate(() => window.dm.server().items.find(i => i.msg?.text === "encrypted server hello"));
  assert.equal(serverRow.sealed.conversation, "server");
  assert.ok(!Buffer.from(serverRow.sealed.ciphertext, "base64url").includes(Buffer.from("encrypted server hello")));
  assert.equal(serverRow.authentic, true);
  await d.getByRole("button", { name: "Who can read", exact: true }).click();
  await d.getByRole("dialog").getByText("Messages are end-to-end encrypted.", { exact: false }).waitFor();
  await d.getByRole("dialog").getByText("User71", { exact: true }).waitFor();
  await d.getByRole("dialog").getByRole("button", { name: "Close", exact: true }).click();
  console.log("PASS encrypted server groups join on connect, seal messages and list readers");
  const d2 = await device(61, 12, encryptedWs);
  await d2.evaluate(() => window.dm.selectServer()); await composer(d2); await sync(d, e, d2);
  await d2.getByText("encrypted server hello", { exact: true }).waitFor({ timeout: 90000 });
  assert.equal(await d2.evaluate(() => window.dm.server().items.find(i => i.msg?.text === "encrypted server hello").authentic), true);
  console.log("PASS a later server-chat device receives earlier messages through history");
  await d.getByRole("button", { name: "Edit message", exact: true }).click();
  await d.getByRole("textbox", { name: "Edit message" }).fill("edited server hello");
  await d.getByRole("button", { name: "Save", exact: true }).click();
  await e.getByText("edited server hello", { exact: false }).waitFor({ timeout: 45000 });
  await d.getByRole("button", { name: "Delete message", exact: true }).click({ modifiers: ["Shift"] });
  await e.waitForFunction(() => window.dm.server().items.some(i => i.sealed?.deleted && i.authentic), null, { timeout: 45000 });
  console.log("PASS sealed server edits and authenticated own deletion");
  // Confirm the recipient read the earlier row before testing one new unread message.
  await e.bringToFront();
  await e.locator("textarea").click();
  await e.waitForFunction(id => window.dm.server().lastRead >= id, serverRow.sealed.id, { timeout: 45000 });
  for (const page of [d, e]) await page.evaluate(async () => {
    const { messageStorage, refreshRevocations, REVOCATION_FRESH_MS } = await import("/src/net/message-storage.ts");
    await messageStorage.mergeFeed([], undefined, Date.now() - REVOCATION_FRESH_MS - 1);
    if ((await messageStorage.revocation("unused", "unused")).fresh) throw new Error("Local revocation feed did not expire");
    await refreshRevocations(undefined, undefined, true);
    if (!(await messageStorage.revocation("unused", "unused")).fresh) throw new Error("Local revocation feed did not refresh");
  });
  await e.evaluate(() => window.dm.select(1));
  await composer(d);
  assert.equal(await d.evaluate(uid => window.dm.mention("server mention", uid, "server"), await e.evaluate(() => window.dm.uid)), true);
  await e.waitForFunction(() => window.dm.server()?.mentions === 1 && window.dm.server()?.unread === 1, null, { timeout: 45000 }).catch(async error => {
    console.error("server mention state", await e.evaluate(() => { const t = window.dm.server(); return { unread: t.unread, mentions: t.mentions, lastRead: t.lastRead,
      rows: t.items.filter(i => i.kind === "msg").map(i => ({ id: i.msg.id, text: i.msg.text, mentions: i.msg.mentions, verified: i.verified, authentic: i.authentic, cannotOpen: i.cannotOpen })) }; }));
    throw error;
  });
  await e.evaluate(() => window.dm.selectServer());
  console.log("PASS server mentions and unread counts follow the sealed conversation");
  const encryptedToken = /redeem this one-time admin token in your client: (\S+)/.exec(encryptedService.output)?.[1];
  assert.ok(encryptedToken);
  await d.evaluate(token => window.dm.redeem(token), encryptedToken);
  await send(e, "remove server message");
  await d.getByText("remove server message", { exact: true }).waitFor({ timeout: 45000 });
  const removalId = await d.evaluate(() => window.dm.server().items.find(i => i.msg?.text === "remove server message").msg.id);
  const removalRow = d.locator(`[data-message="${removalId}"]`);
  await removalRow.hover();
  await removalRow.getByRole("button", { name: "Delete message", exact: true }).click({ modifiers: ["Shift"] });
  await e.waitForFunction(id => !window.dm.server().items.some(i => i.msg?.id === id), removalId, { timeout: 45000 });
  console.log("PASS moderator removal routes by the server conversation");
  await d.locator('input[type="file"]').setInputFiles({ name: "dot.png", mimeType: "image/png", buffer: dot });
  await d.getByText(`${dot.length} B`, { exact: true }).waitFor({ timeout: 45000 });
  await send(d, "server picture");
  const serverPicture = e.getByRole("button", { name: "Open dot.png", exact: true }).locator("img");
  await serverPicture.waitFor({ timeout: 45000 });
  await e.waitForFunction(() => { const img = document.querySelector('button[aria-label="Open dot.png"] img'); return img?.complete && img.naturalWidth === 1; }, null, { timeout: 45000 });
  const serverFile = await e.evaluate(() => window.dm.server().items.find(i => i.msg?.text === "server picture").msg.attachments[0]);
  const serverBytes = Buffer.from(await (await fetch(`http://127.0.0.1:${encryptedPort}${serverFile.url}`)).arrayBuffer());
  assert.equal(serverBytes.length, dot.length + 28);
  assert.ok(!serverBytes.includes(dot.subarray(0, 16)));
  console.log("PASS encrypted server attachments open on another member's device");
  await Promise.all(pages.map(page => page.evaluate(() => window.dm.close())));

} catch (error) {
  // The local services' output explains most failures (in CI there is nothing else to look at).
  for (const child of children) console.error(`--- ${child.spawnargs.join(" ")}\n${child.output}`);
  throw error;
} finally {
  await browser?.close();
  await stopProcesses(children);
  await rm(directory, { recursive: true, force: true });
}
