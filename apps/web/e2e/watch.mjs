#!/usr/bin/env node
/**
 * TeamSpeak watch links in real browsers (docs/screen-share.md, PR 6): a local gwar-server built
 * with `--features test-hooks` serves the built app and the guest watch page, and its stand-in
 * bridge (`GWAR_TEST_WATCH_LINKS`) prints each watch link instead of posting it to TeamSpeak.
 * No TeamSpeak server is involved.
 *
 * Covered:
 * - the acceptance order: the notice and the checkbox are in the preset menu before any link
 *   exists, and the link appears only once the share is live;
 * - guest pages in Chromium and WebKit open the link and decode the share; the sharing bar counts
 *   them as guests;
 * - the heartbeat: both keep watching for `E2E_WATCH_HOLD_MS` (150 s), more than twice the
 *   server's 60 s idle timeout;
 * - the share ends: both pages say so, and the old link then answers like any unknown one;
 * - a share with the checkbox off has no link;
 * - "Turn off" in the sharing bar: the guest page says the link was turned off.
 *
 *   GWAR_SERVER_BIN      server binary with the test hooks (default: target/debug/gwar-server)
 *   E2E_WATCH_HOLD_MS    how long the guests keep watching (default: 150000)
 */
import { stopProcesses } from "./processes.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const HOLD_MS = Number(process.env.E2E_WATCH_HOLD_MS ?? 150_000);
const children = [];
const opened = [];
const checks = [];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function port() {
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  return port;
}
function start(name, command, args, options = {}) {
  const child = spawn(command, args, { cwd: repoRoot, stdio: ["ignore", "pipe", "pipe"], detached: true, ...options });
  child.output = "";
  child.stdout.on("data", b => { child.output += b; });
  child.stderr.on("data", b => { child.output += b; });
  children.push(child);
  return child;
}
async function waitFor(check, what, timeout = 30_000) {
  const started = Date.now();
  for (;;) {
    if (await check()) return Date.now() - started;
    if (Date.now() - started > timeout) throw new Error(`timed out: ${what}`);
    await sleep(150);
  }
}

/** Every peer connection of a page, for the decoder checks; and, for the sharer, a canvas as its screen. */
function pagePatch(share) {
  window.__pcs = [];
  window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
    construct(target, args) { const pc = new target(...args); window.__pcs.push(pc); return pc; },
  });
  if (!share) return;
  const media = navigator.mediaDevices;
  media.getUserMedia = async () => {
    const context = new AudioContext();
    const destination = context.createMediaStreamDestination();
    const oscillator = context.createOscillator();
    oscillator.connect(destination);
    oscillator.start();
    return destination.stream;
  };
  media.enumerateDevices = async () => [];
  // The test hook: what the picker would return, a 640×360 canvas that changes every frame.
  media.getDisplayMedia = async () => {
    const canvas = document.createElement("canvas");
    [canvas.width, canvas.height] = [640, 360];
    canvas.style.cssText = "position:fixed;left:0;bottom:0;width:64px;height:36px;opacity:0.02;pointer-events:none;z-index:-1";
    document.body.appendChild(canvas);
    const ctx = canvas.getContext("2d");
    let n = 0;
    const draw = () => {
      n++;
      ctx.fillStyle = "#777";
      ctx.fillRect(0, 0, 640, 360);
      ctx.fillStyle = `hsl(${(n * 7) % 360} 70% 50%)`;
      ctx.fillRect((n * 9) % 640, 120, 80, 120);
    };
    draw();
    setInterval(draw, 50);
    return canvas.captureStream(20);
  };
}

/** What a page's receivers decode: the inbound video stream with the most frames. */
const decoded = page => page.evaluate(async () => {
  let best = null;
  for (const pc of window.__pcs.filter(pc => pc.connectionState !== "closed")) {
    (await pc.getStats()).forEach(s => {
      if (s.type === "inbound-rtp" && s.kind === "video" && (!best || (s.framesDecoded ?? 0) > best.frames)) {
        best = { frames: s.framesDecoded ?? 0, width: s.frameWidth ?? 0, height: s.frameHeight ?? 0 };
      }
    });
  }
  return best;
});

/** The page keeps decoding the 16:9 canvas: five more frames than when first seen. */
async function keepsDecoding(page, what) {
  let first = null, last = null;
  const ms = await waitFor(async () => {
    last = await decoded(page);
    if (!last || !last.frames || !last.width) return false;
    if (!first) { first = last; return false; }
    return last.frames >= first.frames + 5;
  }, `${what}: ${page.deviceName} decodes (last ${JSON.stringify(last)})`, 30_000);
  assert.ok(Math.abs(last.width / last.height - 16 / 9) < 0.03, `${what}: a ${last.width}x${last.height} frame is not the 640x360 canvas`);
  checks.push({ check: what, page: page.deviceName, ms, size: `${last.width}x${last.height}`, frames: last.frames });
  return last.frames;
}

/**
 * WebKit on Linux paints remote video unreliably (see e2e/screen.mjs, `paintsUnreliably`); there the
 * receiver's decoded frames are the check, and elsewhere the page's video is painted too.
 */
async function painted(page, what) {
  if (page.engine === "webkit" && process.platform === "linux") return;
  await waitFor(() => page.evaluate(() => {
    const v = document.getElementById("video");
    return v && !v.hidden && v.videoWidth > 0 && v.readyState >= 2;
  }), `${what}: ${page.deviceName} paints the video`);
}

const errors = page => page.errors.filter(e => !/Conversation synchronization|favicon|Failed to load resource|Screen capture constraints|WebSocket|Revocation feed unavailable/.test(e));

async function newPage(browser, name, share) {
  const context = await browser.newContext({ locale: "en-US", viewport: { width: 1280, height: 800 } });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  await context.addInitScript({ content: `(${pagePatch})(${JSON.stringify(share)})` });
  const page = await context.newPage();
  page.errors = [];
  page.on("pageerror", e => page.errors.push(String(e)));
  page.on("console", m => { if (m.type() === "warning" || m.type() === "error") page.errors.push(m.text()); });
  page.deviceName = name;
  page.engine = browser.browserType().name();
  opened.push(page);
  return page;
}

async function member(browser, web, server, name) {
  const page = await newPage(browser, name, true);
  await page.goto(web);
  await page.getByLabel("Server address").fill(server);
  await page.getByRole("button", { name: "Settings", exact: true }).first().click();
  await page.getByRole("tab", { name: "Identity", exact: true }).click();
  const nickname = page.getByRole("dialog").getByLabel("Nickname", { exact: true });
  await waitFor(async () => (await nickname.inputValue()) !== "", "identity");
  await nickname.fill(name);
  await page.getByRole("dialog").getByRole("button", { name: "Save", exact: true }).first().click();
  await page.getByText("Saved", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByText("Not in voice", { exact: true }).waitFor({ timeout: 30_000 });
  return page;
}

const controllerCall = (page, fn, arg) => page.evaluate(`(async () => {
  const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { useScreen } = await window.__gwarE2eImport("/src/state/screen.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  return (${fn})(useSession.getState(), useScreen.getState(), controller, ${JSON.stringify(arg ?? null)});
})()`);

async function join(page, channel) {
  await waitFor(async () => {
    if (await controllerCall(page, (s, _, __, ch) => s.me !== null && s.clients[s.me.session]?.channel === ch, channel)) return true;
    await controllerCall(page, (_, __, controller, ch) => controller.joinChannel(ch).catch(() => {}), channel);
    await sleep(400);
    return false;
  }, `${page.deviceName} joins channel ${channel}`, 40_000);
}

const bar = page => page.getByRole("status", { name: "You are sharing your screen" });
const ownShare = page => controllerCall(page, (_, screen) => (screen.own?.phase === "live" ? screen.own.share : null));

/** The links the stand-in bridge printed, by share. */
function links(server) {
  const open = new Map();
  for (const [, share, url] of server.output.matchAll(/^test hook: watch link (\S+) (\S+)$/gm)) open.set(share, url);
  const gone = new Set([...server.output.matchAll(/^test hook: watch link gone (\S+)$/gm)].map(m => m[1]));
  return { open, gone };
}

/** Opens the preset menu, checks the TeamSpeak notice, sets the checkbox and shares. */
async function share(page, server, postLink) {
  const before = links(server).open.size;
  await page.getByRole("button", { name: "Share screen", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Share your screen" });
  await dialog.waitFor({ timeout: 20_000 });
  // The acceptance order: notice and checkbox first, and no link exists while choosing.
  await dialog.getByText("TeamSpeak users in this channel can watch through a link. Anyone with the link can watch.").waitFor();
  const checkbox = dialog.getByRole("checkbox", { name: "Post a watch link to TeamSpeak" });
  assert.equal(await checkbox.isChecked(), true, "the checkbox is on by default");
  await sleep(500);
  assert.equal(links(server).open.size, before, "no link exists before the share is confirmed");
  if (!postLink) await checkbox.uncheck();
  await dialog.getByRole("button", { name: "Share", exact: true }).click();
  await bar(page).getByText(/watching$/).waitFor({ timeout: 30_000 });
  const id = await ownShare(page);
  assert.ok(id, `${page.deviceName} has a live share`);
  return id;
}

async function guest(browser, name, url) {
  const page = await newPage(browser, name, false);
  await page.goto(url);
  return page;
}
const statusLine = page => page.locator("#status");

async function run(browsers, web, ws, token, server) {
  const sharer = await member(browsers.chromium, web, ws, "alice");
  const channel = await controllerCall(sharer, async (_, __, controller, token) => {
    await controller.connection.request("token.redeem", { token });
    return (await controller.createChannel({ name: "Stage", encrypted: false })).id;
  }, token);
  await join(sharer, channel);

  // A share with the link: it appears only once the share is live.
  const first = await share(sharer, server, true);
  await waitFor(() => links(server).open.has(first), "the stand-in bridge gets the link", 10_000);
  const url = links(server).open.get(first);
  assert.match(url, new RegExp(`^${web.replace(/\./g, "\\.")}/watch#[A-Za-z0-9_-]{43}$`), "the link opens this server's watch page");
  checks.push({ check: "link only after the announcement", ok: true });

  const guests = [await guest(browsers.chromium, "guest-chromium", url), await guest(browsers.webkit, "guest-webkit", url)];
  for (const page of guests) {
    await page.getByRole("heading", { name: "alice is sharing their screen" }).waitFor({ timeout: 20_000 });
    await keepsDecoding(page, "guest watches through the link");
    await painted(page, "guest watches through the link");
    // The page never carries the token anywhere but its own fragment.
    assert.equal(await page.evaluate(() => document.referrer), "");
  }
  await sharer.getByTestId("screen-ts-link").getByText("TeamSpeak link on · 2 guests").waitFor({ timeout: 10_000 });
  checks.push({ check: "sharing bar counts guests", ok: true });

  // The heartbeat: more than two idle timeouts later both still watch.
  const frames = await Promise.all(guests.map(decoded));
  await sleep(HOLD_MS);
  for (const [i, page] of guests.entries()) {
    assert.equal(await statusLine(page).textContent(), "", `${page.deviceName} still shows no status after ${HOLD_MS} ms`);
    const later = await keepsDecoding(page, `guest still watching after ${HOLD_MS / 1000} s`);
    assert.ok(later > frames[i].frames, `${page.deviceName} decoded frames during the hold`);
  }

  // The share ends: both pages say so, and the link stops working like any unknown one.
  await bar(sharer).getByRole("button", { name: "Stop sharing" }).click();
  for (const page of guests) await statusLine(page).getByText("The share has ended").waitFor({ timeout: 10_000 });
  await waitFor(() => links(server).gone.has(first), "the stand-in bridge drops the link", 10_000);
  const stale = await guest(browsers.webkit, "guest-stale", url);
  await statusLine(stale).getByText("This link doesn't work: the share has ended or the link was turned off.").waitFor({ timeout: 10_000 });
  checks.push({ check: "share end reaches guests; the old link is unknown", ok: true });

  // With the checkbox off there is no link at all.
  const quiet = await share(sharer, server, false);
  await sleep(1_500);
  assert.equal(links(server).open.has(quiet), false, "no link for a share without the checkbox");
  assert.equal(await sharer.getByTestId("screen-ts-link").count(), 0, "no link line in the sharing bar");
  await bar(sharer).getByRole("button", { name: "Stop sharing" }).click();
  await bar(sharer).waitFor({ state: "detached" });
  checks.push({ check: "checkbox off: no link", ok: true });

  // Turning the link off reaches its guest at once; the share goes on.
  const third = await share(sharer, server, true);
  await waitFor(() => links(server).open.has(third), "the third share's link", 10_000);
  const late = await guest(browsers.chromium, "guest-turned-off", links(server).open.get(third));
  await keepsDecoding(late, "guest of the third share");
  await sharer.getByTestId("screen-ts-link").getByText("TeamSpeak link on · 1 guest").waitFor({ timeout: 10_000 });
  await sharer.getByTestId("screen-ts-link").getByRole("button", { name: "Turn off" }).click();
  await statusLine(late).getByText("This link was turned off").waitFor({ timeout: 10_000 });
  await sharer.getByTestId("screen-ts-link").waitFor({ state: "detached", timeout: 10_000 });
  await waitFor(() => links(server).gone.has(third), "the turned-off link is dropped", 10_000);
  assert.ok(await ownShare(sharer), "the share goes on without its link");
  checks.push({ check: "turn off reaches the guest", ok: true });

  return opened.flatMap(p => errors(p).map(e => `${p.deviceName}: ${e}`));
}

const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-watch-e2e-"));
const browsers = {};
let failure = null, warnings = [], server = null;
try {
  const dist = path.join(dataDir, "dist");
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", dist], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, `e2e web build failed:\n${build.stdout}${build.stderr}`);
  const binary = process.env.GWAR_SERVER_BIN ?? path.join(repoRoot, "target/debug/gwar-server");
  const serverPort = await port();
  const web = `http://127.0.0.1:${serverPort}`;
  const ws = `ws://127.0.0.1:${serverPort}/ws`;
  server = start("gwar-server", binary, [
    "--data-dir", path.join(dataDir, "server"), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`,
    "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--public-url", web, "--web-root", dist, "--no-connect",
  ], { env: { ...process.env, GWAR_TEST_WATCH_LINKS: "1" } });
  await waitFor(() => /redeem this one-time admin token in your client: (\S+)/.test(server.output), "server admin token", 60_000);
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)[1];
  browsers.chromium = await chromium.launch({ headless: true, args: ["--autoplay-policy=no-user-gesture-required"] });
  browsers.webkit = await webkit.launch({ headless: true });
  warnings = await run(browsers, web, ws, token, server);
} catch (error) {
  failure = error;
  for (const page of opened) {
    const status = await page.locator("#status").textContent({ timeout: 1_000 }).catch(() => null);
    console.error(JSON.stringify({ page: page.deviceName, url: page.url().replace(/#.*/, "#…"), status, decoded: await decoded(page).catch(e => String(e)), errors: page.errors.slice(-20) }));
  }
  if (server) console.error(server.output.split("\n").filter(l => !l.includes("watch link")).slice(-40).join("\n"));
} finally {
  for (const browser of Object.values(browsers)) await browser.close().catch(() => {});
  await stopProcesses(children);
  rmSync(dataDir, { recursive: true, force: true });
}
for (const check of checks) console.log(JSON.stringify(check));
if (warnings.length) console.log(JSON.stringify({ warnings }));
if (failure) { console.error(failure.stack ?? failure); process.exit(1); }
console.log(JSON.stringify({ status: "pass" }));
