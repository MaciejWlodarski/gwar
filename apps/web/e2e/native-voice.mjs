#!/usr/bin/env node
/**
 * Encrypted voice with the native engine (docs/e2e-build.md "Tests and
 * acceptance", 5d): a real local gwar-server, the built app and its real MLS
 * controller, and desktop devices whose `NativeVoiceEngine` drives the Rust
 * engine in `native_bridge` (the desktop shell's command contract, virtual
 * 10 ms devices) instead of Tauri. One browser device uses WebRTC + SFrame
 * workers. Tones are checked at the browser's WebAudio output and at each
 * native speaker.
 *
 * Build once: cargo build -p gwar-server && cargo build -p gwar-client --example native_bridge
 *   E2E_BROWSERS=chromium,webkit   engine of every page (default: chromium)
 */
import { stopProcesses } from "./processes.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const engines = (process.env.E2E_BROWSERS ?? "chromium").split(",").map(s => s.trim()).filter(Boolean);
const bridgeBinary = process.env.NATIVE_BRIDGE_BIN ?? path.join(repoRoot, "target/debug/examples/native_bridge");
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
function start(command, args, options = {}) {
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

/** One native engine process: JSON commands in, replies and events out. */
function nativeFixture(frequency) {
  const child = spawn(bridgeBinary, [String(frequency)], { cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"], detached: true });
  children.push(child);
  child.log = "";
  child.stderr.on("data", b => { child.log += b; });
  const pending = new Map();
  let next = 0;
  const fixture = { page: null, log: () => child.log };
  createInterface({ input: child.stdout }).on("line", line => {
    const message = JSON.parse(line);
    child.log += `< ${line.slice(0, 300)}\n`;
    if ("event" in message) {
      void fixture.page?.evaluate(({ event, payload }) => window.__nativeEmit(event, payload), message).catch(() => {});
      return;
    }
    const waiter = pending.get(message.id);
    pending.delete(message.id);
    if ("error" in message) waiter?.reject(new Error(message.error));
    else waiter?.resolve(message.ok);
  });
  fixture.request = (command, args = {}) => new Promise((resolve, reject) => {
    const id = ++next;
    pending.set(id, { resolve, reject });
    if (command !== "snapshot" && command !== "audio_devices") child.log += `> ${command} ${JSON.stringify(args).slice(0, 300)}\n`;
    child.stdin.write(`${JSON.stringify({ id, command, args })}\n`);
  });
  fixture.hears = async other => (await fixture.request("snapshot", { frequency: other })).tone > 0.05;
  return fixture;
}

/** Browser-side half of the bridge: `NativeVoiceEngine`'s invoke/listen pair. */
function bridgePatch() {
  const listeners = new Map();
  window.__nativeEmit = (event, payload) => { for (const handler of listeners.get(event) ?? []) handler(payload); };
  window.__nativeBridge = async () => ({
    invoke: (command, args) => window.__nativeInvoke(command, args ?? {}),
    listen: async (event, handler) => {
      const set = listeners.get(event) ?? new Set();
      listeners.set(event, set);
      set.add(handler);
      return () => set.delete(handler);
    },
  });
}

/** Synthetic microphone and the app's output tap for the browser device. */
function devicePatch(frequency) {
  window.__outputs = [];
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

async function browserTone(page, frequency) {
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
const hears = async (device, frequency) => device.native ? device.native.hears(frequency) : (await browserTone(device.page, frequency)) > 0.05;

const voice = page => page.evaluate(async () => {
  const { useVoice } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  const { state, encryption, hearers } = useVoice.getState();
  return { state, encryption, hearers, debug: controller.voiceDebug() };
});
const counters = page => page.evaluate(async () => {
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  return controller.voiceCounters();
});

async function device(browser, web, server, name, frequency, native) {
  const context = await browser.newContext({ locale: "en-US" });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  await context.addInitScript(devicePatch, frequency);
  const page = await context.newPage();
  const fixture = native ? nativeFixture(frequency) : null;
  if (fixture) {
    fixture.page = page;
    await page.exposeFunction("__nativeInvoke", (command, args) => fixture.request(command, args));
    await context.addInitScript(bridgePatch);
  }
  page.errors = [];
  page.on("pageerror", e => page.errors.push(String(e)));
  page.on("console", m => { if (m.type() === "warning" || m.type() === "error") page.errors.push(m.text()); });
  await page.goto(web);
  if (fixture) {
    // The desktop app's engine, driven through the fixture instead of Tauri.
    await page.waitForFunction(() => typeof window.__gwarE2eImport === "function");
    await page.evaluate(async () => {
      const { NativeVoiceEngine } = await window.__gwarE2eImport("/src/voice/native-engine.ts");
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      controller.useEngine(new NativeVoiceEngine(window.__nativeBridge));
    });
  }
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
  const d = { name, frequency, page, native: fixture };
  opened.push(d);
  return d;
}

/** Joins voice in the channel, retrying while this device's channel group is still being joined. */
async function join(d, channel) {
  return waitFor(async () => {
    const inVoice = await d.page.evaluate(async channel => {
      const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
      const s = useSession.getState();
      return s.me !== null && s.clients[s.me.session]?.channel === channel;
    }, channel);
    if (inVoice) return true;
    await d.page.evaluate(async channel => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.joinChannel(channel).catch(() => {});
    }, channel);
    await sleep(500);
    return false;
  }, `${d.name} enters the channel`, 40_000);
}
const leave = d => d.page.evaluate(async () => {
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  await controller.leaveVoice();
});
const active = (d, what) => waitFor(async () => (await voice(d.page)).encryption === "active", `${d.name} ${what}`, 30_000);

async function run(engine, web, server, token) {
  const type = engine === "webkit" ? webkit : chromium;
  const browser = await type.launch({ headless: true, args: engine === "chromium" ? ["--autoplay-policy=no-user-gesture-required"] : [] });
  try {
    const timings = {};
    const alice = await device(browser, web, server, `alice-${engine}`, 440, false);
    await alice.page.evaluate(async token => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.connection.request("token.redeem", { token });
    }, token);
    const channel = await alice.page.evaluate(async () => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      return (await controller.createChannel({ name: "secret", encrypted: true, history: true })).id;
    });
    const dan = await device(browser, web, server, `dan-native-${engine}`, 550, true);
    const erin = await device(browser, web, server, `erin-native-${engine}`, 770, true);

    timings.aliceEnter = await join(alice, channel);
    timings.aliceActive = await active(alice, "sending key active");

    // Browser <-> native through the SFU, both directions SFrame.
    timings.danEnter = await join(dan, channel);
    timings.danActive = await active(dan, "native sending key active");
    timings.danHearsAlice = await waitFor(() => hears(dan, 440), "native dan hears browser alice", 20_000);
    timings.aliceHearsDan = await waitFor(() => hears(alice, 550), "browser alice hears native dan", 20_000);

    // Native <-> native, plus both natives with the browser.
    timings.erinEnter = await join(erin, channel);
    timings.erinActive = await active(erin, "native sending key active");
    timings.erinHears = await waitFor(async () => (await hears(erin, 440)) && (await hears(erin, 550)), "native erin hears alice and dan", 25_000);
    timings.danHearsErin = await waitFor(() => hears(dan, 770), "native dan hears native erin", 25_000);
    timings.aliceHearsErin = await waitFor(() => hears(alice, 770), "browser alice hears native erin", 25_000);

    // Real SFrame on every engine: both directions counted, nothing failed.
    for (const d of [alice, dan, erin]) {
      await waitFor(async () => { const c = await counters(d.page); return c && c.encrypted > 50 && c.decrypted > 50; }, `${d.name} SFrame counters`, 10_000);
      const c = await counters(d.page);
      assert.equal(c.auth_failed + c.own_kid_dropped + c.unexpected_duration, 0, `${d.name} SFrame failures ${JSON.stringify(c)}`);
      timings[`${d.name}Counters`] = c;
    }
    for (const d of [alice, dan, erin]) assert.equal((await voice(d.page)).hearers?.members.length, 3, `${d.name} lists three hearers`);

    // A native removal: the others keep talking, dan's tone stops everywhere.
    await leave(dan);
    await waitFor(async () => (await voice(erin.page)).hearers?.members.length === 2, "dan removed from erin's voice tree", 30_000);
    await waitFor(async () => (await hears(erin, 440)) && !(await hears(erin, 550)), "erin hears alice without dan", 25_000);
    await waitFor(async () => (await hears(alice, 770)) && !(await hears(alice, 550)), "alice hears erin without dan", 25_000);
    await active(erin, "reactivated after the removal");

    // Leaving clears the call; a plaintext control channel still works natively.
    await leave(alice); await leave(erin);
    const plain = await alice.page.evaluate(async () => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      return (await controller.createChannel({ name: "open", encrypted: false })).id;
    });
    await join(alice, plain); await join(erin, plain);
    await waitFor(() => hears(erin, 440), "plaintext control: native erin hears alice", 20_000);
    assert.equal((await voice(erin.page)).encryption, null);
    assert.equal(await counters(erin.page), null, "no SFrame counters in plaintext");

    const errors = opened.flatMap(d => d.page.errors.filter(e => !/Conversation synchronization|favicon|Failed to load resource/.test(e)).map(e => `${d.name}: ${e}`));
    results.push({ engine, status: "pass", timings, errors });
  } catch (error) {
    const devices = [];
    for (const d of opened) devices.push({ device: d.name, voice: await voice(d.page).catch(e => String(e)), errors: d.page.errors.slice(-25), native: d.native?.log().slice(-6000) });
    error.devices = devices;
    throw error;
  } finally { opened.length = 0; await browser.close(); }
}

const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-native-voice-e2e-"));
try {
  const webPort = await port();
  const web = `http://127.0.0.1:${webPort}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", path.join(dataDir, "dist")], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, `e2e web build failed:\n${build.stdout}${build.stderr}`);
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start(process.execPath, [viteCli, "preview", "--outDir", path.join(dataDir, "dist"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web preview", 30_000);
  const binary = process.env.GWAR_SERVER_BIN ?? path.join(repoRoot, "target/debug/gwar-server");
  for (const engine of engines) {
    const serverPort = await port();
    const ws = `ws://127.0.0.1:${serverPort}/ws`;
    const server = start(binary, ["--data-dir", path.join(dataDir, `server-${engine}`), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"]);
    await waitFor(() => /redeem this one-time admin token in your client: (\S+)/.test(server.output), "server admin token", 60_000);
    const token = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)[1];
    try { await run(engine, web, ws, token); }
    catch (error) { results.push({ engine, status: "fail", error: String(error.stack ?? error), devices: error.devices }); }
  }
} finally {
  await stopProcesses(children);
  rmSync(dataDir, { recursive: true, force: true });
}
for (const r of results) console.log(JSON.stringify(r));
if (results.some(r => r.status === "fail")) process.exit(1);
