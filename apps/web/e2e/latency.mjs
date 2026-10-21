#!/usr/bin/env node
/**
 * Mouth-to-ear latency of the web client (docs/latency.md "Web client"): the marker scheme of
 * crates/gwar-client/examples/latency.rs, sent from a synthetic microphone in the built app and
 * detected at the receiving app's master output, through a fresh loopback server per run.
 *
 * Build first: cargo build --release -p gwar-client --example web_latency
 * Run:   pnpm --filter web latency [--variant NAME:key=value,...]... [--set key=value,...]
 *        [--runs 4] [--markers 60] [--warmup-ms 5000] [--out DIR]
 * Keys:  path=browser|browser-native|native-browser (sender-receiver; default browser)
 *        engine=chromium|webkit|firefox   crypto=plain|sframe (browser path only)
 *        ns=off|on (the sending app's noise suppression)
 *        net=idle|relay|jitter|loss|<LATENCY_NET spec> (relay in front of every media client)
 *        mic=webaudio|track (the synthetic microphone: an AudioWorklet's MediaStream destination,
 *          or, Chromium only, a MediaStreamTrackGenerator fed 10 ms AudioData from the page)
 *        ptime=MS (adds a=ptime to the sender's answer: a harness experiment, not the app)
 *        jbt=MS (the receiver's RTCRtpReceiver.jitterBufferTarget: also an experiment)
 * Variants run interleaved, alternating supplied and reversed order (A B B A ...), each run
 * with a fresh server, relay and browser. Output: target/web-latency/<time>/, one directory per
 * run (run.json, latency.csv, logs) and summary.json. A run fails, and is kept, if more than
 * 5% of its markers are lost; nothing is retried or excluded.
 */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { setTimeout as wait } from "node:timers/promises";
import { chromium, firefox, webkit } from "playwright";
import { stopProcesses } from "./processes.mjs";
import {
  callbackSizes, detectOnsets, GENERATOR_WORKLET, MARKER, MATCH_WINDOW_MS, matchMarkers, PERIOD, quantile, RATE, renderClock, TAP_WORKLET, TONES,
} from "./latency-markers.mjs";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const NETWORKS = { idle: null, relay: "seed=1", jitter: "seed=42;down:jitter=10ms", loss: "seed=42;down:loss=2%" };
const DEFAULTS = { path: "browser", engine: "chromium", crypto: "plain", ns: "off", net: "idle", mic: "webaudio", ptime: null, jbt: null };
const MINIMUM_MS = 5;

function parseArgs(argv) {
  const options = { runs: 4, markers: 60, warmupMs: 5000, out: null, variants: [], set: {} };
  const pairs = text => Object.fromEntries(text.split(",").filter(Boolean).map(pair => {
    const at = pair.indexOf("=");
    assert(at > 0, `expected key=value: ${pair}`);
    const key = pair.slice(0, at);
    assert(key in DEFAULTS, `unknown key ${key}`);
    return [key, pair.slice(at + 1)];
  }));
  for (let i = 0; i < argv.length; i++) {
    const value = () => { assert(i + 1 < argv.length, `${argv[i]} needs a value`); return argv[++i]; };
    switch (argv[i]) {
      case "--": break;
      case "--variant": {
        const text = value();
        const colon = text.indexOf(":");
        const name = colon < 0 ? text : text.slice(0, colon);
        assert(/^[\w.-]+$/.test(name), `variant name ${name}`);
        options.variants.push({ name, keys: pairs(colon < 0 ? "" : text.slice(colon + 1)) });
        break;
      }
      case "--set": Object.assign(options.set, pairs(value())); break;
      case "--runs": options.runs = Number(value()); break;
      case "--markers": options.markers = Number(value()); break;
      case "--warmup-ms": options.warmupMs = Number(value()); break;
      case "--out": options.out = path.resolve(value()); break;
      default: throw new Error(`unknown argument ${argv[i]}`);
    }
  }
  assert(Number.isInteger(options.runs) && options.runs > 0, "--runs must be a positive integer");
  assert(Number.isInteger(options.markers) && options.markers > 0, "--markers must be a positive integer");
  assert(Number.isInteger(options.warmupMs) && options.warmupMs >= 0 && options.warmupMs <= 30_000, "--warmup-ms must be 0..30000");
  if (options.variants.length === 0) options.variants.push({ name: "default", keys: {} });
  assert.equal(new Set(options.variants.map(v => v.name)).size, options.variants.length, "variant names must differ");
  for (const variant of options.variants) {
    const c = variant.config = { ...DEFAULTS, ...options.set, ...variant.keys };
    assert(["browser", "browser-native", "native-browser"].includes(c.path), `path ${c.path}`);
    assert(["chromium", "webkit", "firefox"].includes(c.engine), `engine ${c.engine}`);
    assert(["plain", "sframe"].includes(c.crypto), `crypto ${c.crypto}`);
    assert(c.crypto === "plain" || c.path === "browser", "sframe needs path=browser");
    assert(["off", "on"].includes(c.ns), `ns ${c.ns}`);
    assert(["webaudio", "track"].includes(c.mic), `mic ${c.mic}`);
    assert(c.mic === "webaudio" || c.engine === "chromium", "mic=track needs Chromium");
    assert(c.ns === "off" || c.path !== "native-browser", "ns applies to a browser sender");
    for (const key of ["ptime", "jbt"]) if (c[key] !== null) { c[key] = Number(c[key]); assert(Number.isFinite(c[key]) && c[key] >= 0, key); }
    c.netSpec = c.net in NETWORKS ? NETWORKS[c.net] : c.net;
  }
  return options;
}

const options = parseArgs(process.argv.slice(2));
const started = new Date();
const out = options.out ?? path.join(repoRoot, "target/web-latency", `${started.toISOString().replaceAll(":", "-")}-${process.pid}`);
assert(!existsSync(out), `${out} already exists`);
mkdirSync(out, { recursive: true });
const scratch = mkdtempSync(path.join(os.tmpdir(), "gwar-web-latency-"));
const children = [];

async function port() {
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  return port;
}
function start(command, args, options = {}) {
  const child = spawn(command, args, { cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"], detached: true, ...options });
  child.log = "";
  child.stderr.on("data", b => { child.log += b; });
  children.push(child);
  return child;
}
async function waitFor(check, what, timeout = 20_000) {
  const begin = Date.now();
  for (;;) {
    if (await check()) return Date.now() - begin;
    if (Date.now() - begin > timeout) throw new Error(`timed out: ${what}`);
    await sleep(100);
  }
}
async function deadline(promise, ms, what) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`timed out: ${what}`)), ms); })]);
  } finally { clearTimeout(timer); }
}
const sha256 = file => createHash("sha256").update(readFileSync(file)).digest("hex");

/** Host load: the machine is shared, so every run records it. */
function load() {
  const ps = spawnSync("ps", ["-axo", "%cpu=,comm="], { encoding: "utf8" }).stdout ?? "";
  const busy = ps.split("\n").map(line => line.trim().split(/\s+/, 2)).filter(([cpu]) => Number(cpu) >= 50);
  const builds = busy.filter(([, comm]) => /(^|\/)(rustc|cargo|clang|cc|ld|ld64)$/.test(comm ?? "")).length;
  return { load1: os.loadavg()[0], busyProcesses: busy.length, busyBuildProcesses: builds };
}

// ------------------------------------------------------------------ the page

/** Init script: the synthetic microphone, peer connections, the output tap and experiments. */
function pagePatch({ role, ptime, mic, scheme }) {
  const state = window.__latency = { role, pcs: [] };
  const NativePC = window.RTCPeerConnection;
  window.RTCPeerConnection = new Proxy(NativePC, { construct(target, args) {
    const pc = new target(...args);
    state.pcs.push(pc);
    if (role === "receiver" && typeof window.MediaStreamTrackProcessor === "function") pc.addEventListener("track", e => decoded(e.track));
    return pc;
  } });
  // Chromium: decoded audio as remote tracks deliver it to their sinks, before the app's WebAudio
  // graph. A slot's frames are kept once one carries sound (only the sender's slot does).
  state.decoded = [];
  const decoded = async track => {
    const slot = { chunks: [], audible: false };
    state.decoded.push(slot);
    const reader = new window.MediaStreamTrackProcessor({ track }).readable.getReader();
    for (;;) {
      const { value: frame, done } = await reader.read();
      if (done) return;
      if (state.recording) {
        const ms = performance.timeOrigin + performance.now();
        const samples = new Float32Array(frame.numberOfFrames);
        frame.copyTo(samples, { planeIndex: 0, format: "f32-planar" });
        slot.audible ||= samples.some(x => Math.abs(x) > 1e-3);
        if (slot.audible) slot.chunks.push({ ms, timestamp: frame.timestamp, rate: frame.sampleRate, samples });
      }
      frame.close();
    }
  };
  if (ptime) {
    // The microphone is the first media section; the answer's a=ptime sets the encoder's frame.
    const set = NativePC.prototype.setRemoteDescription;
    NativePC.prototype.setRemoteDescription = function (description) {
      if (description?.type === "answer") {
        const [head, first, ...rest] = description.sdp.split(/\r\n(?=m=)/);
        description = { type: "answer", sdp: [head, first.replace(/\r\n$/, "") + `\r\na=ptime:${ptime}`, ...rest].join("\r\n") };
      }
      return set.call(this, description);
    };
  }
  const connect = AudioNode.prototype.connect;
  let generator = null;
  // The same signal as the generator worklet, written as 10 ms AudioData when it is due; each
  // frame's samples are timed from its write, like latency.rs's virtual capture callbacks.
  const trackGenerator = () => {
    const { RATE, PERIOD, MARKER, TONES } = scheme;
    const track = new window.MediaStreamTrackGenerator({ kind: "audio" });
    const writer = track.writable.getWriter();
    const g = { track, captures: [], arm: null, start: null, collect: async () => ({ captures: g.captures, sampleRate: RATE }) };
    g.arm = arm => { g.armed = arm; };
    const begin = performance.now();
    let position = 0;
    const tick = async () => {
      while (begin + position * 1000 / RATE <= performance.now()) {
        const now = performance.timeOrigin + performance.now();
        if (g.armed && g.start === null) g.start = position + g.armed.warmup;
        const data = new Float32Array(480);
        for (let i = 0; i < data.length; i++) {
          const at = position + i;
          const since = g.start === null ? -1 : at - g.start;
          const index = Math.floor(since / PERIOD);
          const inMarker = since >= 0 && index < g.armed.count && since % PERIOD < MARKER;
          if (since >= 0 && index < g.armed.count && since % PERIOD === 0) g.captures.push({ capture_ms: now + i * 1000 / RATE, identity: index % TONES.length });
          const frequency = inMarker ? TONES[index % TONES.length] : 400;
          data[i] = role === "sender" ? Math.sin(at * frequency * 2 * Math.PI / RATE) * (inMarker ? 0.6 : 0.08) : 0;
        }
        await writer.write(new AudioData({ format: "f32-planar", sampleRate: RATE, numberOfFrames: 480, numberOfChannels: 1, timestamp: position * 1e6 / RATE, data }));
        position += 480;
      }
      setTimeout(tick, 1);
    };
    void tick();
    return (state.generator = g);
  };
  navigator.mediaDevices.getUserMedia = async () => {
    if (mic === "track") generator ??= Promise.resolve(trackGenerator());
    generator ??= (async () => {
      const ctx = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
      await ctx.audioWorklet.addModule("/__latency/generator.js");
      const node = new AudioWorkletNode(ctx, "latency-generator", { numberOfInputs: 0, outputChannelCount: [1], processorOptions: { silent: role !== "sender" } });
      const destination = ctx.createMediaStreamDestination();
      destination.channelCount = 1;
      connect.call(node, destination);
      await ctx.resume();
      const collect = () => new Promise(resolve => { node.port.onmessage = e => resolve(e.data); node.port.postMessage({ collect: true }); });
      return (state.generator = { ctx, node, track: destination.stream.getAudioTracks()[0], arm: arm => node.port.postMessage({ arm }), collect });
    })();
    // Each capture is a clone: the app stops its tracks, the generator keeps running.
    return new MediaStream([(await generator).track.clone()]);
  };
  navigator.mediaDevices.enumerateDevices = async () => [];
  // Taps record, in parallel, what the app connects to its output (the receiver's master gain)
  // and, on the sender, what its noise suppression sends (the model's worklet, which feeds a
  // MediaStream destination of its own context).
  state.taps = {};
  const tap = (name, node) => {
    let t = state.taps[name];
    if (t?.ctx !== node.context) {
      const ctx = node.context;
      t = state.taps[name] = { ctx, input: ctx.createGain() };
      t.ready = ctx.audioWorklet.addModule("/__latency/tap.js").then(() => {
        t.node = new AudioWorkletNode(ctx, "latency-tap", { numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 1, channelCountMode: "explicit" });
        const mute = ctx.createGain();
        mute.gain.value = 0;
        connect.call(t.input, t.node);
        connect.call(t.node, mute);
        connect.call(mute, ctx.destination);
      });
    }
    connect.call(node, t.input);
  };
  AudioNode.prototype.connect = function (...args) {
    const target = args[0];
    if (role === "receiver" && target === this.context?.destination) tap("output", this);
    if (role === "sender" && target instanceof MediaStreamAudioDestinationNode) tap("sent", this);
    return connect.apply(this, args);
  };
}

/** WebRTC and audio-context statistics; reported as stats, never as measured latency. */
function pageStats() {
  return (async () => {
    const state = window.__latency;
    const pc = state.pcs.at(-1);
    const report = pc ? [...(await pc.getStats()).values()] : [];
    const by = id => report.find(s => s.id === id);
    const inbound = report.filter(s => s.type === "inbound-rtp" && s.kind === "audio").sort((a, b) => (b.packetsReceived ?? 0) - (a.packetsReceived ?? 0))[0] ?? null;
    const outbound = report.find(s => s.type === "outbound-rtp" && s.kind === "audio") ?? null;
    const pair = report.find(s => s.type === "candidate-pair" && s.nominated && s.state === "succeeded") ?? null;
    const context = ctx => ctx && {
      sampleRate: ctx.sampleRate, state: ctx.state, baseLatency: ctx.baseLatency ?? null, outputLatency: ctx.outputLatency ?? null,
      playbackStats: ctx.playbackStats ? JSON.parse(JSON.stringify(ctx.playbackStats)) : null,
    };
    const receivers = pc ? pc.getReceivers().map(r => ({ jitterBufferTarget: r.jitterBufferTarget ?? null, supported: "jitterBufferTarget" in r })) : [];
    return {
      at: Date.now(), connectionState: pc?.connectionState ?? null,
      inbound, inboundCodec: inbound ? by(inbound.codecId) ?? null : null,
      outbound, outboundCodec: outbound ? by(outbound.codecId) ?? null : null,
      mediaSource: outbound ? by(outbound.mediaSourceId) ?? null : null,
      playout: report.filter(s => s.type === "media-playout"),
      roundTripMs: pair?.currentRoundTripTime != null ? pair.currentRoundTripTime * 1000 : null,
      outputContext: context(state.taps.output?.ctx), sentContext: context(state.taps.sent?.ctx), microphoneContext: context(state.generator?.ctx),
      receiverDelay: receivers[1] ?? null,
    };
  })();
}

async function device(browser, web, url, role, config) {
  const context = await browser.newContext({ locale: "en-US" });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  await context.addInitScript(pagePatch, { role, ptime: role === "sender" ? config.ptime : null, mic: config.mic, scheme: { RATE, PERIOD, MARKER, TONES } });
  const page = await context.newPage();
  page.errors = [];
  page.on("pageerror", e => page.errors.push(String(e)));
  page.on("console", m => { if (m.type() === "warning" || m.type() === "error") page.errors.push(m.text()); });
  await page.goto(web);
  await page.getByLabel("Server address").fill(url);
  await page.getByRole("button", { name: "Settings", exact: true }).first().click();
  await page.getByRole("tab", { name: "Identity", exact: true }).click();
  const nickname = page.getByRole("dialog").getByLabel("Nickname", { exact: true });
  await waitFor(async () => (await nickname.inputValue()) !== "", "identity");
  await nickname.fill(role);
  await page.getByRole("dialog").getByRole("button", { name: "Save", exact: true }).first().click();
  await page.getByText("Saved", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByText("Not in voice", { exact: true }).waitFor({ timeout: 20_000 });
  // The browser's own capture processing does not apply to the synthetic microphone; the app's
  // noise suppression (its model) does. At full strength the model rightly removes the steady
  // marker tones; at 10% (a 5 dB cap) they pass. libDF mixes the captured spectrum back into the
  // same frame before synthesis, so the delay is the same at every strength.
  await page.evaluate(async ns => {
    const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
    useSettings.getState().setAudio({ noiseSuppression: ns, noiseStrength: 10, echoCancellation: false, autoGainControl: false });
  }, role === "sender" && config.ns === "on");
  page.role = role;
  return page;
}

async function join(page, channel) {
  await waitFor(async () => {
    const inChannel = await page.evaluate(async channel => {
      const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
      const s = useSession.getState();
      return s.me !== null && s.clients[s.me.session]?.channel === channel;
    }, channel);
    if (inChannel) return true;
    await page.evaluate(async channel => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.joinChannel(channel).catch(() => {});
    }, channel);
    await sleep(500);
    return false;
  }, `${page.role} joins the channel`, 40_000);
}
const voiceState = page => page.evaluate(async () => {
  const { useVoice } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  const engine = controller.engine;
  const sent = window.__latency.pcs.at(-1)?.getSenders()[0]?.track ?? null;
  return {
    state: useVoice.getState().state, encryption: useVoice.getState().encryption,
    denoise: engine.denoise ? { running: engine.denoise.running, sent: sent === engine.denoise.track } : null,
    denoiseUnavailable: engine.denoiseUnavailable ?? null,
  };
});

// --------------------------------------------------------------- one run

async function fixtureProcess(binary, web, dir, netSpec) {
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("LATENCY_") && !key.startsWith("GWAR_")));
  if (netSpec) env.LATENCY_NET = netSpec;
  const child = start(binary, [web, dir], { env });
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  child.response = async (event, timeout = 30_000) => {
    const line = await deadline(lines.next(), timeout, `fixture ${event}`);
    assert(!line.done, `fixture exited before ${event}: ${child.log.slice(-2000)}`);
    const value = JSON.parse(line.value);
    assert.equal(value.event, event);
    return value;
  };
  child.command = (command, event = command.split(" ")[0], timeout) => { child.stdin.write(`${command}\n`); return child.response(event, timeout); };
  return child;
}

async function collectTap(page, name) {
  if (!(await page.evaluate(name => Boolean(window.__latency.taps[name]?.node), name))) return null;
  return page.evaluate(async name => {
    const tap = window.__latency.taps[name].node;
    const data = await new Promise(resolve => { tap.port.onmessage = e => resolve(e.data); tap.port.postMessage({ collect: true }); });
    const bytes = new Uint8Array(data.samples.buffer);
    let binary = "";
    for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return { startFrame: data.startFrame, sampleRate: data.sampleRate, clock: data.clock, pcm: btoa(binary) };
  }, name);
}
const collectGenerator = page => page.evaluate(() => window.__latency.generator.collect());

/**
 * One run. `owned` lets the caller's timeout close the browser and cut the run's waits short,
 * so a timed-out run cannot overlap the next.
 */
async function measure({ config, web, binary, dir, markers, warmupMs, record, owned }) {
  const pause = ms => wait(ms, undefined, { signal: owned.abort.signal });
  const fixture = await fixtureProcess(binary, web, dir, config.netSpec);
  const ready = await fixture.response("ready");
  record.fixture = { url: ready.url, media: ready.media, mediaBind: ready.mediaBind };
  const engine = { chromium, webkit, firefox }[config.engine];
  const browser = await engine.launch({
    headless: true,
    args: config.engine === "chromium" ? ["--autoplay-policy=no-user-gesture-required"] : [],
    firefoxUserPrefs: { "media.autoplay.default": 0, "media.autoplay.blocking_policy": 0 },
  });
  owned.browser = browser;
  record.browserVersion = browser.version();
  try {
    owned.abort.signal.throwIfAborted();
    const sender = config.path === "native-browser" ? null : await device(browser, web, ready.url, "sender", config);
    const receiver = config.path === "browser-native" ? null : await device(browser, web, ready.url, "receiver", config);
    const pages = [sender, receiver].filter(Boolean);
    let channel = await pages[0].evaluate(async () => {
      const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
      return useSession.getState().server.default_channel;
    });
    if (config.crypto === "sframe") {
      await sender.evaluate(async token => {
        const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
        await controller.connection.request("token.redeem", { token });
      }, ready.adminToken);
      channel = await sender.evaluate(async () => {
        const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
        return (await controller.createChannel({ name: "latency", encrypted: true, history: false })).id;
      });
    }
    for (const page of pages) await join(page, channel);
    if (config.path !== "browser") await fixture.command(config.path === "browser-native" ? "receive" : "send");
    for (const page of pages) {
      await waitFor(async () => (await voiceState(page)).state === "connected", `${page.role} voice connected`, 30_000);
      if (config.crypto === "sframe") await waitFor(async () => (await voiceState(page)).encryption === "active", `${page.role} SFrame active`, 30_000);
    }
    if (config.path !== "browser") await waitFor(async () => (await fixture.command("link")).link === "Connected", "native voice connected", 30_000);
    if (receiver) {
      await receiver.evaluate(() => window.__latency.taps.output.ready);
      // Audio flows: a second of decoded samples from the sender.
      await waitFor(async () => ((await receiver.evaluate(pageStats)).inbound?.totalSamplesReceived ?? 0) > 48_000, "audio at the receiver", 30_000);
      if (config.jbt !== null) {
        record.jitterBufferTarget = await receiver.evaluate(ms => window.__latency.pcs.at(-1).getReceivers().map(r => {
          if (!("jitterBufferTarget" in r)) return null;
          r.jitterBufferTarget = ms;
          return r.jitterBufferTarget;
        }), config.jbt);
      }
    }
    if (config.ns === "on") {
      record.denoiseLoadMs = await waitFor(async () => {
        const v = await voiceState(sender);
        if (v.denoiseUnavailable) throw new Error("noise suppression is unavailable in this browser");
        return v.denoise?.running && v.denoise.sent;
      }, "the noise suppression model sends", 60_000);
    }
    record.voice = Object.fromEntries(await Promise.all(pages.map(async page => [page.role, await voiceState(page)])));

    // Arm: the receiver records, the sender starts markers after the warmup.
    for (const page of pages) {
      await page.evaluate(async () => {
        window.__latency.recording = true;
        for (const t of Object.values(window.__latency.taps)) {
          await t.ready;
          t.node.port.postMessage({ record: true });
        }
      });
    }
    const warmupFrames = Math.round(warmupMs * RATE / 1000);
    if (sender) await sender.evaluate(arm => window.__latency.generator.arm(arm), { warmup: warmupFrames, count: markers });
    if (config.path !== "browser") record.armMs = (await fixture.command(`arm ${warmupMs} ${markers}`)).atMs;
    record.load.armed = load();
    await pause(warmupMs);
    const before = Object.fromEntries(await Promise.all(pages.map(async page => [page.role, await page.evaluate(pageStats)])));
    await pause(markers * PERIOD * 1000 / RATE + MATCH_WINDOW_MS + 500);
    const after = Object.fromEntries(await Promise.all(pages.map(async page => [page.role, await page.evaluate(pageStats)])));
    record.stats = { before, after };
    record.load.end = load();
    const generator = sender ? await collectGenerator(sender) : null;
    const tap = receiver ? await collectTap(receiver, "output") : null;
    const sent = sender ? await collectTap(sender, "sent") : null;
    const decoded = receiver ? await receiver.evaluate(() => {
      window.__latency.recording = false;
      const slot = window.__latency.decoded.find(s => s.audible);
      if (!slot) return null;
      const all = new Float32Array(slot.chunks.reduce((n, c) => n + c.samples.length, 0));
      let at = 0;
      for (const c of slot.chunks) { all.set(c.samples, at); at += c.samples.length; }
      const bytes = new Uint8Array(all.buffer);
      let binary = "";
      for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
      return { rate: slot.chunks[0]?.rate, origin: performance.timeOrigin, chunks: slot.chunks.map(c => [c.ms, c.samples.length, c.timestamp]), pcm: btoa(binary) };
    }) : null;
    const finish = await fixture.command("finish", "finish", 60_000);
    record.native = finish.native && { ...finish.native, blocks: undefined };
    record.network = finish.network;
    record.sframe = config.crypto === "sframe" ? Object.fromEntries(await Promise.all(pages.map(async page => [page.role, await page.evaluate(async () => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      return controller.voiceCounters();
    })]))) : null;
    record.pageErrors = Object.fromEntries(pages.map(page => [page.role, page.errors.filter(e => !/favicon|Failed to load resource|Conversation synchronization/.test(e)).slice(-20)]));
    fixture.stdin.end("quit\n");
    return { generator, tap, sent, decoded, native: finish.native };
  } finally {
    await browser.close();
  }
}

/** A tap's samples, timed on its context's render clock. */
function tapped(tap) {
  const bytes = Buffer.from(tap.pcm, "base64");
  const at = renderClock(tap.clock, tap.sampleRate);
  return {
    samples: new Float32Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length)),
    rate: tap.sampleRate, timeOf: i => at(tap.startFrame + i), callbacks: callbackSizes(tap.clock),
  };
}

/** Latency quantiles of one point of the path, against the same captures (a segment, not the gate). */
function segment(captures, samples, rate, timeOf) {
  const m = matchMarkers(captures, detectOnsets(samples, rate, timeOf), 0);
  const l = m.matched.map(x => x.latency_ms).sort((a, b) => a - b);
  return { n: l.length, median_ms: quantile(l, 0.5), p95_ms: quantile(l, 0.95) };
}

/**
 * Decoded audio as the remote track delivers it. Chromium stamps each `AudioData` with its
 * delivery time, in microseconds since the page's time origin; the page reads it later, on its
 * main thread. Frames are timed from their stamps placed directly on the measurement clock
 * (`performance.timeOrigin` + stamp), so main-thread delay does not enter. The mapping is only
 * used once it passes a plausibility check: no frame may be read before its stamp, and the
 * median read lag must stay under `STAMP_LAG_MS`. Otherwise frames are timed from their reads and
 * `timing` says so. The check cannot separate an epoch offset from read delay, so the segment is
 * always `calibrated: false`.
 * Absolute read lags and the read-timed result are kept for comparison. Samples follow their
 * frame's time, like a capture callback's.
 */
const STAMP_LAG_MS = 20;
const STAMP_SLACK_MS = 0.5;
function decodedSegment(captures, decoded) {
  const bytes = Buffer.from(decoded.pcm, "base64");
  const all = new Float32Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length));
  const { chunks, rate, origin } = decoded;
  const increasing = chunks.every(([, , ts], i) => Number.isFinite(ts) && (i === 0 || ts > chunks[i - 1][2]));
  const stampTimes = chunks.map(([, , ts]) => origin + ts / 1000);
  const lags = chunks.map(([ms], i) => ms - stampTimes[i]).sort((a, b) => a - b);
  const lag = { min: lags[0], median: quantile(lags, 0.5), p95: quantile(lags, 0.95), max: lags.at(-1) };
  const stamped = Number.isFinite(origin) && increasing && lag.min >= -STAMP_SLACK_MS && lag.median <= STAMP_LAG_MS;
  const starts = [];
  let length = 0;
  for (const [, n] of chunks) { starts.push(length); length += n; }
  const timed = times => i => {
    let low = 0, high = starts.length - 1;
    while (low < high) { const mid = (low + high + 1) >> 1; if (starts[mid] <= i) low = mid; else high = mid - 1; }
    return times[low] + (i - starts[low]) * 1000 / rate;
  };
  const read = segment(captures, all, rate, timed(chunks.map(([ms]) => ms)));
  const result = stamped ? segment(captures, all, rate, timed(stampTimes)) : read;
  const steps = new Map();
  chunks.slice(1).forEach(([, , ts], i) => { const step = Math.round((ts - chunks[i][2]) / 1000); steps.set(step, (steps.get(step) ?? 0) + 1); });
  return {
    // The stamp epoch is assumed, not measured: a constant offset would pass the check.
    ...result, timing: stamped ? "timestamp" : "read", calibrated: false, frameSamples: chunks[0][1],
    readTimedMedianMs: read.median_ms,
    // Read time minus stamp time, per frame, on the measurement clock.
    readLagMs: lag,
    timestampStepsMs: Object.fromEntries([...steps].sort((a, b) => b[1] - a[1]).slice(0, 8)),
  };
}

function analyze({ record, generator, tap, sent, decoded, native, dir, markers }) {
  // Captures: the browser generator's frames on its render clock, or the native callback times.
  let captures;
  if (generator?.clock) {
    const at = renderClock(generator.clock, generator.sampleRate);
    captures = generator.captures.map(c => ({ capture_ms: at(c.frame), identity: c.identity }));
    record.microphoneCallbacks = callbackSizes(generator.clock);
  } else captures = generator ? generator.captures : native.captures;
  assert.equal(captures.length, markers, `only ${captures.length} of ${markers} markers were generated`);
  let samples, rate, timeOf;
  if (sent) {
    // A segment, not the gate: microphone to the noise suppression's output, in the sending page.
    const t = tapped(sent);
    record.sentSegment = { ...segment(captures, t.samples, t.rate, t.timeOf), callbacks: t.callbacks };
  }
  if (decoded?.chunks.length) record.decodedSegment = decodedSegment(captures, decoded);
  if (tap) {
    let callbacks;
    ({ samples, rate, timeOf, callbacks } = tapped(tap));
    record.outputCallbacks = callbacks;
  } else {
    const bytes = readFileSync(path.join(dir, "native-playout.f32"));
    samples = new Float32Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.length));
    rate = RATE;
    const blocks = native.blocks;
    timeOf = i => {
      let low = 0, high = blocks.length - 1;
      while (low < high) { const mid = (low + high + 1) >> 1; if (blocks[mid][0] <= i) low = mid; else high = mid - 1; }
      return blocks[low][1] + (i - blocks[low][0]) * 1000 / rate;
    };
  }
  const onsets = detectOnsets(samples, rate, timeOf);
  const matching = matchMarkers(captures, onsets, MINIMUM_MS);
  const latencies = matching.matched.map(m => m.latency_ms).sort((a, b) => a - b);
  writeFileSync(path.join(dir, "latency.csv"), "marker,identity,capture_ms,playout_ms,latency_ms,playout_sample\n" + matching.matched.map(m => {
    const c = captures[m.marker], o = onsets[m.onset];
    return `${m.marker},${c.identity},${c.capture_ms.toFixed(3)},${o.playout_ms.toFixed(3)},${m.latency_ms.toFixed(3)},${o.sample}`;
  }).join("\n") + "\n");
  Object.assign(record, {
    markers, n: latencies.length, lost_markers: matching.lost, rejected_onsets: matching.rejected,
    ambiguous_onsets: onsets.flatMap((o, i) => (o.identity === null ? [i] : [])), unmatched_onsets: matching.unused,
    onsets: onsets.length, median_ms: quantile(latencies, 0.5), p95_ms: quantile(latencies, 0.95), p99_ms: quantile(latencies, 0.99),
    latencies_ms: matching.matched.map(m => m.latency_ms), marker_captures: captures, detected_onsets: onsets,
  });
  record.derived = derive(record);
  // NetEq's delay changes between runs (its target adapts); without it, runs compare the rest.
  if (record.median_ms !== null && Number.isFinite(record.derived.jitterBufferMs)) record.derived.medianLessJitterBufferMs = record.median_ms - record.derived.jitterBufferMs;
  if (matching.rejected.length) throw new Error(`rejected ${matching.rejected.length} onsets below ${MINIMUM_MS} ms`);
  if (matching.lost.length > markers / 20) throw new Error(`lost ${matching.lost.length} of ${markers} markers; at least 95% must match`);
}

/** Window differences of the receiver's and sender's counters. */
function derive(record) {
  const { before, after } = record.stats;
  const result = {};
  const r0 = before.receiver?.inbound, r1 = after.receiver?.inbound;
  if (r0 && r1) {
    const d = key => (r1[key] ?? NaN) - (r0[key] ?? NaN);
    const emitted = d("jitterBufferEmittedCount");
    Object.assign(result, {
      packetsReceived: d("packetsReceived"), packetsLost: d("packetsLost"),
      receivedPacketMs: d("totalSamplesReceived") / d("packetsReceived") / 48,
      jitterBufferMs: d("jitterBufferDelay") / emitted * 1000,
      jitterBufferTargetMs: d("jitterBufferTargetDelay") / emitted * 1000,
      jitterBufferMinimumMs: d("jitterBufferMinimumDelay") / emitted * 1000,
      concealmentPercent: d("concealedSamples") / d("totalSamplesReceived") * 100,
      concealmentEvents: d("concealmentEvents"),
      acceleratedSamples: d("removedSamplesForAcceleration"), deceleratedSamples: d("insertedSamplesForDeceleration"),
      jitterMs: r1.jitter != null ? r1.jitter * 1000 : null,
    });
  }
  const o = after.receiver?.outputContext, o0 = before.receiver?.outputContext;
  if (o) {
    Object.assign(result, { outputBaseLatencyMs: o.baseLatency * 1000, outputLatencyMs: o.outputLatency != null ? o.outputLatency * 1000 : null });
    if (o.playbackStats && o0?.playbackStats) {
      result.outputUnderrunEvents = o.playbackStats.underrunEvents - o0.playbackStats.underrunEvents;
      result.outputUnderrunMs = (o.playbackStats.underrunDuration - o0.playbackStats.underrunDuration) * 1000;
      result.outputAverageLatencyMs = o.playbackStats.averageLatency * 1000;
    }
  }
  const s0 = before.sender?.outbound, s1 = after.sender?.outbound;
  if (s0 && s1) {
    const seconds = (after.sender.at - before.sender.at) / 1000;
    result.senderPacketsPerSecond = (s1.packetsSent - s0.packetsSent) / seconds;
    result.senderFrameMs = 1000 / result.senderPacketsPerSecond;
    const m = after.sender.microphoneContext;
    if (m) Object.assign(result, { microphoneBaseLatencyMs: m.baseLatency * 1000, microphoneOutputLatencyMs: m.outputLatency != null ? m.outputLatency * 1000 : null });
  }
  if (record.native?.stats) {
    const s = record.native.stats;
    Object.assign(result, { nativeUnderruns: s.underruns, nativeUnderrunSamples: s.underrun_samples, nativeSlots: s.slots });
  }
  if (record.network) result.relay = { flows: record.network.flows, up: record.network.up, down: record.network.down, downlinkSendErrors: record.network.downlink_send_errors };
  return result;
}

function summarize(runs, variants) {
  const median = values => { const v = values.filter(Number.isFinite).sort((a, b) => a - b); return v.length ? quantile(v, 0.5) : null; };
  const rows = variants.map(variant => {
    const mine = runs.filter(run => run.variant === variant.name);
    const pooled = mine.flatMap(run => run.latencies_ms ?? []).sort((a, b) => a - b);
    const d = key => median(mine.map(run => run.derived?.[key]));
    return {
      variant: variant.name, config: variant.config, runs: mine.length, passed: mine.filter(run => run.passed).length,
      matched: mine.reduce((sum, run) => sum + (run.n ?? 0), 0), markers: mine.reduce((sum, run) => sum + (run.markers ?? 0), 0),
      median_ms: quantile(pooled, 0.5), p95_ms: quantile(pooled, 0.95), p99_ms: quantile(pooled, 0.99),
      run_medians_ms: mine.map(run => run.median_ms),
      stats: {
        jitterBufferMs: d("jitterBufferMs"), medianLessJitterBufferMs: d("medianLessJitterBufferMs"), sentSegmentMs: median(mine.map(run => run.sentSegment?.median_ms)), decodedSegmentMs: median(mine.map(run => run.decodedSegment?.median_ms)), jitterBufferTargetMs: d("jitterBufferTargetMs"), concealmentPercent: d("concealmentPercent"),
        packetsLost: mine.reduce((sum, run) => sum + (run.derived?.packetsLost ?? 0), 0),
        senderFrameMs: d("senderFrameMs"), outputLatencyMs: d("outputLatencyMs"), outputBaseLatencyMs: d("outputBaseLatencyMs"),
        outputUnderrunEvents: mine.reduce((sum, run) => sum + (run.derived?.outputUnderrunEvents ?? 0), 0),
        nativeUnderruns: mine.reduce((sum, run) => sum + (run.derived?.nativeUnderruns ?? 0), 0),
      },
      load1: median(mine.map(run => run.load?.armed?.load1)),
    };
  });
  for (const row of rows) row.median_delta_ms = row.median_ms !== null && rows[0].median_ms !== null ? row.median_ms - rows[0].median_ms : null;
  return rows;
}

async function main() {
  const binary = path.resolve(repoRoot, process.env.LATENCY_WEB_FIXTURE ?? "target/release/examples/web_latency");
  assert(existsSync(binary), "build the fixture first: cargo build --release -p gwar-client --example web_latency");
  const manifest = {
    started: started.toISOString(), revision: spawnSync("git", ["rev-parse", "HEAD"], { cwd: repoRoot, encoding: "utf8" }).stdout.trim(),
    fixture: path.relative(repoRoot, binary), fixtureSha256: sha256(binary), harnessSha256: sha256(fileURLToPath(import.meta.url)),
    host: { platform: `${os.platform()} ${os.release()}`, cpus: os.cpus()[0]?.model, cpuCount: os.cpus().length },
    options: { runs: options.runs, markers: options.markers, warmupMs: options.warmupMs, variants: options.variants }, order: [],
  };
  const dist = path.join(scratch, "dist");
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", dist], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  writeFileSync(path.join(out, "build.log"), `${build.stdout}${build.stderr}`);
  assert.equal(build.status, 0, "e2e web build failed; see build.log");
  mkdirSync(path.join(dist, "__latency"));
  writeFileSync(path.join(dist, "__latency/generator.js"), GENERATOR_WORKLET);
  writeFileSync(path.join(dist, "__latency/tap.js"), TAP_WORKLET);
  const web = `http://127.0.0.1:${await port()}`;
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start(process.execPath, [viteCli, "preview", "--outDir", dist, "--host", "127.0.0.1", "--port", new URL(web).port, "--strictPort"], { cwd: webRoot });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web preview", 30_000);

  // Setup, warmup, markers, drain and collection; setup allows for 60 s waits on the model.
  const budget = 180_000 + options.warmupMs + options.markers * PERIOD * 1000 / RATE + MATCH_WINDOW_MS;
  const runs = [];
  for (let cycle = 0; cycle < options.runs; cycle++) {
    const order = cycle % 2 === 0 ? options.variants : options.variants.toReversed();
    for (const variant of order) {
      const index = runs.length + 1;
      const dir = path.join(out, `${String(index).padStart(3, "0")}-${variant.name}`);
      mkdirSync(dir);
      const record = { variant: variant.name, cycle, config: variant.config, started: new Date().toISOString(), load: { start: load() } };
      manifest.order.push(path.basename(dir));
      const childCount = children.length;
      const owned = { browser: null, abort: new AbortController() };
      const work = measure({ config: variant.config, web, binary, dir, markers: options.markers, warmupMs: options.warmupMs, record, owned });
      try {
        analyze({ ...(await deadline(work, budget, "run")), record, dir, markers: options.markers });
        record.passed = true;
      } catch (error) {
        record.passed = false;
        record.error = String(error.stack ?? error);
      }
      // After a timeout the run is still going: cancel its waits, close its browser and
      // processes, and let it end before the next run starts.
      owned.abort.abort();
      await owned.browser?.close().catch(() => {});
      const runChildren = children.splice(childCount);
      await stopProcesses(runChildren).catch(error => { record.cleanupError = String(error); });
      await work.catch(() => {});
      writeFileSync(path.join(dir, "fixture.log"), runChildren.map(child => child.log).join("\n"));
      rmSync(path.join(dir, "native-playout.f32"), { force: true });
      writeFileSync(path.join(dir, "run.json"), JSON.stringify(record, null, 1));
      runs.push(record);
      const d = record.derived ?? {};
      console.log(`${index} ${variant.name}: ${record.passed ? "ok" : "FAIL"} n=${record.n ?? "-"}/${options.markers} median=${record.median_ms?.toFixed(1) ?? "-"} p95=${record.p95_ms?.toFixed(1) ?? "-"} ms` +
        ` neteq=${d.jitterBufferMs?.toFixed(1) ?? "-"} ms load=${record.load.armed?.load1.toFixed(1) ?? "-"}${record.passed ? "" : ` ${record.error.split("\n")[0]}`}`);
    }
  }
  const summary = summarize(runs, options.variants);
  writeFileSync(path.join(out, "manifest.json"), JSON.stringify(manifest, null, 2));
  writeFileSync(path.join(out, "summary.json"), JSON.stringify(summary, null, 2));
  const f = value => (Number.isFinite(value) ? value.toFixed(2) : "-");
  for (const row of summary) {
    console.log(`${row.variant}: ${row.passed}/${row.runs} runs, ${row.matched}/${row.markers} markers, median ${f(row.median_ms)} p95 ${f(row.p95_ms)} p99 ${f(row.p99_ms)} ms` +
      ` (delta ${f(row.median_delta_ms)}), NetEq ${f(row.stats.jitterBufferMs)} ms, less NetEq ${f(row.stats.medianLessJitterBufferMs)} ms,` +
      ` frame ${f(row.stats.senderFrameMs)} ms, load ${f(row.load1)}`);
  }
  console.log(`Evidence: ${out}`);
  if (runs.some(run => !run.passed)) process.exitCode = 1;
}

try {
  await main();
} catch (error) {
  console.error(error.stack ?? error);
  process.exitCode = 1;
} finally {
  await stopProcesses(children).catch(error => console.error(String(error)));
  rmSync(scratch, { recursive: true, force: true });
}
