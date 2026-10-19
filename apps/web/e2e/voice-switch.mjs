#!/usr/bin/env node
/**
 * Moving between plaintext and encrypted voice with one click (live-test regression), then:
 * encrypted voice in real browsers (docs/e2e-build.md "Tests and acceptance",
 * 5c): a real local gwar-server, the built app, SFrame workers and synthetic
 * microphone tones observed at each receiver's WebAudio output.
 *
 *   E2E_BROWSERS=chromium,webkit   engines to run (default: both)
 *
 * A browser without RTCRtpScriptTransform is reported as an unrun check, not a pass.
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
const engines = (process.env.E2E_BROWSERS ?? "chromium,webkit").split(",").map(s => s.trim()).filter(Boolean);
const children = [];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const results = [];
const opened = [];

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
async function waitFor(check, what, timeout = 20_000) {
  const started = Date.now();
  for (;;) {
    if (await check()) return Date.now() - started;
    if (Date.now() - started > timeout) throw new Error(`timed out: ${what}`);
    await sleep(100);
  }
}

/** Synthetic microphone tone per device, the app's output tap, and peer connections. */
function devicePatch(frequency) {
  window.__pcs = [];
  window.__outputs = [];
  const NativePC = window.RTCPeerConnection;
  window.RTCPeerConnection = new Proxy(NativePC, { construct(target, args) { const pc = new target(...args); window.__pcs.push(pc); return pc; } });
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (...args) {
    if (args[0] === this.context.destination) {
      const analyser = this.context.createAnalyser();
      analyser.fftSize = 8192;
      connect.call(this, analyser);
      window.__outputs.push(analyser);
    }
    return connect.apply(this, args);
  };
  const media = navigator.mediaDevices;
  media.getUserMedia = async () => {
    const context = new AudioContext();
    const oscillator = context.createOscillator();
    oscillator.frequency.value = frequency;
    const gain = context.createGain();
    gain.gain.value = 0.3;
    const destination = context.createMediaStreamDestination();
    oscillator.connect(gain).connect(destination);
    oscillator.start();
    void context.resume();
    return destination.stream;
  };
  media.enumerateDevices = async () => [];
}

/** Energy of one frequency in the receiver's speaker output (0..1 of all energy). */
async function tone(page, frequency) {
  return page.evaluate(frequency => {
    const analyser = window.__outputs.at(-1);
    if (!analyser) return 0;
    const pcm = new Float32Array(analyser.fftSize);
    analyser.getFloatTimeDomainData(pcm);
    let energy = 0, sine = 0, cosine = 0;
    for (let i = 0; i < pcm.length; i++) {
      energy += pcm[i] * pcm[i];
      const phase = i * frequency * 2 * Math.PI / analyser.context.sampleRate;
      sine += pcm[i] * Math.sin(phase);
      cosine += pcm[i] * Math.cos(phase);
    }
    const rms = Math.sqrt(energy / pcm.length);
    return rms < 0.002 ? 0 : 2 * (sine * sine + cosine * cosine) / (pcm.length * energy);
  }, frequency);
}
const hears = async (page, frequency) => (await tone(page, frequency)) > 0.05;

const voice = page => page.evaluate(async () => {
  const { useVoice } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  const { state, encryption, hearers } = useVoice.getState();
  return { state, encryption, hearers, debug: controller.voiceDebug() };
});

async function device(browser, web, server, name, frequency) {
  const context = await browser.newContext({ locale: "en-US" });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  await context.addInitScript(devicePatch, frequency);
  const page = await context.newPage();
  page.errors = [];
  page.on("pageerror", e => page.errors.push(String(e)));
  page.on("console", m => { if (m.type() === "warning" || m.type() === "error") page.errors.push(m.text()); });
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
  await page.getByText("Not in voice", { exact: true }).waitFor({ timeout: 20_000 });
  await page.evaluate(async () => {
    const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
    useSettings.getState().setAudio({ noiseSuppression: false, echoCancellation: false, autoGainControl: false });
  });
  page.deviceName = name; page.frequency = frequency; page.context_ = context;
  opened.push(page);
  return page;
}

/** Joins voice in the channel, retrying while this device's channel group is still being joined. */
async function join(page, channel) {
  const started = Date.now();
  await waitFor(async () => {
    const inVoice = await page.evaluate(async channel => {
      const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
      const s = useSession.getState();
      return s.me !== null && s.clients[s.me.session]?.channel === channel;
    }, channel);
    if (inVoice) return true;
    await page.evaluate(async channel => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.joinChannel(channel).catch(() => {});
    }, channel);
    await sleep(500);
    return false;
  }, `${page.deviceName} enters the channel`, 40_000);
  return Date.now() - started;
}

async function run(engine, web, server, token) {
  const type = engine === "webkit" ? webkit : chromium;
  const browser = await type.launch({ headless: true, args: engine === "chromium" ? ["--autoplay-policy=no-user-gesture-required"] : [] });
  try {
    const alice = await device(browser, web, server, `alice-${engine}`, 440);
    await alice.evaluate(async token => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.connection.request("token.redeem", { token });
    }, token);
    const plain = await alice.evaluate(async () => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      return (await controller.createChannel({ name: "open", encrypted: false })).id;
    });
    const bob = await device(browser, web, server, `bob-${engine}`, 660);
    await join(alice, plain); await join(bob, plain);
    await waitFor(() => hears(bob, 440), "plaintext: bob hears alice", 20_000);
    // One click each, no retries, as a person does.
    const click = (page, channel) => page.evaluate(async channel => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.joinChannelInteractive(channel);
    }, channel);
    const active = page => waitFor(async () => (await voice(page)).encryption === "active", `${page.deviceName} encrypted voice active`, 30_000);
    // An encrypted channel made while both are connected: bob enters it at once, before the
    // background pass would have joined its group.
    const secret = await alice.evaluate(async () => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      return (await controller.createChannel({ name: "secret", encrypted: true, history: true })).id;
    });
    await click(bob, secret); await active(bob);
    await click(alice, secret); await active(alice);
    await waitFor(async () => (await hears(alice, 660)) && (await hears(bob, 440)), "encrypted: both hear each other", 20_000);
    console.log("PASS a new encrypted channel is entered with one click from a plaintext one");
    // Back to plaintext and into the encrypted channel again, twice (a new call each time).
    for (const round of [1, 2]) {
      await click(alice, plain); await click(bob, plain);
      await waitFor(async () => (await voice(alice)).encryption === null && (await voice(bob)).encryption === null, "back in plaintext", 20_000);
      await click(alice, secret); await click(bob, secret);
      await active(alice); await active(bob);
      await waitFor(async () => (await hears(alice, 660)) && (await hears(bob, 440)), `round ${round}: both hear each other`, 20_000);
    }
    console.log("PASS moving between plaintext and encrypted voice repeatedly");
    results.push({ engine, status: "pass" });
  } finally { opened.length = 0; await browser.close(); }
}

const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-voice-e2e-"));
try {
  const webPort = await port();
  const web = `http://127.0.0.1:${webPort}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", path.join(dataDir, "dist")], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, `e2e web build failed:\n${build.stdout}${build.stderr}`);
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start("vite", process.execPath, [viteCli, "preview", "--outDir", path.join(dataDir, "dist"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web preview", 30_000);
  const binary = process.env.GWAR_SERVER_BIN ?? path.join(repoRoot, "target/debug/gwar-server");
  for (const engine of engines) {
    // A fresh server per engine: its own admin token, calls and groups.
    const serverPort = await port();
    const ws = `ws://127.0.0.1:${serverPort}/ws`;
    const server = start("gwar-server", binary, ["--data-dir", path.join(dataDir, `server-${engine}`), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"]);
    await waitFor(() => /redeem this one-time admin token in your client: (\S+)/.test(server.output), "server admin token", 60_000);
    const token = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)[1];
    try { await run(engine, web, ws, token); }
    catch (error) { results.push({ engine, status: "fail", error: String(error.stack ?? error), pages: error.pages }); }
  }
} finally {
  await stopProcesses(children);
  rmSync(dataDir, { recursive: true, force: true });
}
for (const r of results) console.log(JSON.stringify(r));
if (results.some(r => r.status === "fail")) process.exit(1);
