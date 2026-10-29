#!/usr/bin/env node
/**
 * Real native 10 ms Opus -> Chromium playback, browser -> native playback,
 * and speaker reassignment on the same receive slot. Loopback services only.
 *
 * Build once: cargo build -p gwar-client --example web_audio
 * Run: pnpm --filter web e2e:native-audio
 * JSON snapshots, SDP, process logs and generated fake-mic WAV are retained in
 * target/web-audio/<run>. The default takes about 30 s, excluding builds.
 * E2E_AUDIO_SCENARIO=reuse|pause|continuous|prewarmed, E2E_AUDIO_SENDER=native|browser,
 * E2E_AUDIO_PRE_SECONDS / POST_SECONDS and E2E_AUDIO_GAP_MS select research controls.
 * E2E_AUDIO_RESEARCH=1 retains quality checks but removes the 60 ms delay ceiling.
 */
import { chooseFirstRunName } from "./first-run.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const out = path.join(repoRoot, "target/web-audio", `${new Date().toISOString().replaceAll(":", "-")}-${process.pid}`);
mkdirSync(out, { recursive: true });
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const children = [];
const report = { revision: spawnSync("git", ["rev-parse", "HEAD"], { cwd: repoRoot, encoding: "utf8" }).stdout.trim(), phases: [], reassignments: [], slotEvents: [], errors: [] };
let browser;
const runStarted = Date.now();
const scenario = process.env.E2E_AUDIO_SCENARIO ?? "reuse";
assert(["reuse", "pause", "continuous", "prewarmed"].includes(scenario), "unknown audio scenario");
const preSeconds = Number(process.env.E2E_AUDIO_PRE_SECONDS ?? 10);
const postSeconds = Number(process.env.E2E_AUDIO_POST_SECONDS ?? 10);
const gapMs = Number(process.env.E2E_AUDIO_GAP_MS ?? 2200);
for (const seconds of [preSeconds, postSeconds]) assert(Number.isInteger(seconds) && seconds > 0 && seconds <= 120);
assert(Number.isFinite(gapMs) && gapMs >= 0 && gapMs <= 60000);
const research = process.env.E2E_AUDIO_RESEARCH === "1";
const sender = process.env.E2E_AUDIO_SENDER ?? "native";
assert(["native", "browser"].includes(sender), "unknown audio sender");
assert(sender === "native" || scenario !== "prewarmed", "prewarmed control uses the native fixture");
const packetSamples = sender === "native" ? 480 : 960;
report.options = { scenario, sender, preSeconds, postSeconds, gapMs, research };

async function deadline(promise, ms, what) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`timeout waiting for ${what}`)), ms);
    })]);
  } finally { clearTimeout(timer); }
}

async function waitFor(fn, what, timeout = 15000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await fn()) return;
    await sleep(100);
  }
  throw new Error(`timeout waiting for ${what}`);
}

function start(name, executable, args, options = {}) {
  const child = spawn(executable, args, { cwd: repoRoot, ...options });
  let log = "";
  child.stdout.on("data", (data) => { log += data; });
  child.stderr.on("data", (data) => { log += data; });
  child.on("error", (error) => { log += error.stack; });
  children.push({ name, child, log: () => log });
  return child;
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  await deadline(exited, 3000, "process exit").catch(() => {});
  if (child.exitCode === null && child.signalCode === null) {
    child.kill("SIGKILL");
    await exited;
  }
}

async function port() {
  const probe = net.createServer();
  await new Promise((resolve) => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
}

function microphoneWav() {
  // One whole second, so Chromium can loop the 700 Hz tone without a discontinuity.
  const rate = 48000;
  const wav = Buffer.alloc(44 + rate * 2);
  wav.write("RIFF", 0); wav.writeUInt32LE(wav.length - 8, 4); wav.write("WAVEfmt ", 8);
  wav.writeUInt32LE(16, 16); wav.writeUInt16LE(1, 20); wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(rate, 24); wav.writeUInt32LE(rate * 2, 28);
  wav.writeUInt16LE(2, 32); wav.writeUInt16LE(16, 34);
  wav.write("data", 36); wav.writeUInt32LE(rate * 2, 40);
  for (let i = 0; i < rate; i++) wav.writeInt16LE(Math.round(0.25 * 32767 * Math.sin(i * 700 * 2 * Math.PI / rate)), 44 + i * 2);
  const file = path.join(out, "microphone.wav");
  writeFileSync(file, wav);
  return file;
}

function spy() {
  window.__pcs = [];
  window.__outputs = [];
  const NativePC = window.RTCPeerConnection;
  window.RTCPeerConnection = new Proxy(NativePC, {
    construct(target, args) {
      const pc = new target(...args);
      window.__pcs.push(pc);
      return pc;
    },
  });
  // Observe the actual app master -> speaker path, after its per-user/master gains.
  // Keep the original output connected and add only a parallel analyser.
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (...args) {
    if (args[0] === this.context.destination) {
      const analyser = this.context.createAnalyser();
      analyser.fftSize = 4096;
      connect.call(this, analyser);
      window.__outputs.push(analyser);
    }
    return connect.apply(this, args);
  };
}

async function outputTone(page, frequency) {
  return page.evaluate((frequency) => {
    const analyser = window.__outputs.at(-1);
    if (!analyser) return null;
    const pcm = new Float32Array(analyser.fftSize);
    analyser.getFloatTimeDomainData(pcm);
    let energy = 0, sine = 0, cosine = 0;
    for (let i = 0; i < pcm.length; i++) {
      energy += pcm[i] * pcm[i];
      const phase = i * frequency * 2 * Math.PI / analyser.context.sampleRate;
      sine += pcm[i] * Math.sin(phase);
      cosine += pcm[i] * Math.cos(phase);
    }
    return {
      rms: Math.sqrt(energy / pcm.length),
      toneFraction: energy > 0 ? 2 * (sine * sine + cosine * cosine) / (pcm.length * energy) : 0,
      state: analyser.context.state,
      sampleRate: analyser.context.sampleRate,
    };
  }, frequency);
}

async function stats(page, slot) {
  return page.evaluate(async (slot) => {
    const pc = window.__pcs.at(-1);
    const transceiver = pc?.getTransceivers()[slot + 1];
    if (!transceiver) return null;
    const all = await pc.getStats();
    const inbound = [...all.values()].find((s) => s.type === "inbound-rtp" && s.kind === "audio" && s.mid === transceiver.mid);
    if (!inbound) return null;
    return {
      inbound: { ...inbound },
      codec: { ...all.get(inbound.codecId) },
      mid: transceiver.mid,
      trackId: transceiver.receiver.track.id,
      pcCount: window.__pcs.length,
      receiverDelayControls: {
        jitterBufferTargetSupported: "jitterBufferTarget" in transceiver.receiver,
        jitterBufferTarget: transceiver.receiver.jitterBufferTarget ?? null,
        playoutDelayHintSupported: "playoutDelayHint" in transceiver.receiver,
        playoutDelayHint: transceiver.receiver.playoutDelayHint ?? null,
      },
      state: pc.connectionState,
      offer: pc.localDescription.sdp,
      answer: pc.remoteDescription.sdp,
    };
  }, slot);
}

async function connect(page, url) {
  await chooseFirstRunName(page, "browser 700 Hz");
  await page.getByLabel("Server address").fill(url);
  await page.getByRole("button", { name: "Settings", exact: true }).first().click();
  await page.getByRole("tab", { name: "Identity", exact: true }).click();
  const nickname = page.getByRole("main", { name: "Settings" }).getByLabel("Nickname", { exact: true });
  await waitFor(async () => (await nickname.inputValue()) !== "", "identity initialization");
  await nickname.fill("browser 700 Hz");
  await page.getByRole("main", { name: "Settings" }).getByRole("button", { name: "Save", exact: true }).first().click();
  await page.getByText("Saved", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByText("Not in voice", { exact: true }).waitFor();
  // Use the existing built-app test module access to select a reproducible capture
  // profile. This avoids noise suppression treating a steady test tone as noise.
  await page.evaluate(async () => {
    const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
    useSettings.getState().setAudio({ noiseSuppression: false, echoCancellation: false, autoGainControl: false });
  });
  await page.getByRole("treeitem").first().hover();
  await page.getByRole("button", { name: /^Join voice in / }).first().click();
  await waitFor(() => page.evaluate(() => window.__pcs.at(-1)?.connectionState === "connected"), "browser ICE");
}

function summarize(phase) {
  const a = phase.before.inbound, b = phase.after.inbound;
  const delta = (key) => {
    assert(Number.isFinite(a[key]) && Number.isFinite(b[key]), `missing inbound stat ${key}`);
    return b[key] - a[key];
  };
  const duration = (b.timestamp - a.timestamp) / 1000;
  const samples = delta("totalSamplesReceived");
  const emitted = delta("jitterBufferEmittedCount");
  assert(samples > 0 && emitted > 0, "no browser decoded samples");
  const n = phase.nativeAfter, p = phase.nativeBefore;
  const nativeSamples = n.samples - p.samples;
  const nativeEnergy = n.energy - p.energy;
  return {
    speaker: phase.frequency,
    slot: phase.slot,
    seconds: duration,
    packetsReceived: delta("packetsReceived"),
    packetsLost: delta("packetsLost"),
    packetsPerSecond: delta("packetsReceived") / duration,
    jitterMs: b.jitter * 1000,
    concealedSamples: delta("concealedSamples"),
    totalSamplesReceived: samples,
    concealmentPercent: delta("concealedSamples") / samples * 100,
    jitterBufferDelay: delta("jitterBufferDelay"),
    jitterBufferEmittedCount: emitted,
    jitterBufferMs: delta("jitterBufferDelay") / emitted * 1000,
    jitterBufferTargetMs: delta("jitterBufferTargetDelay") / emitted * 1000,
    jitterBufferMinimumMs: delta("jitterBufferMinimumDelay") / emitted * 1000,
    samplesPerPacket: samples / delta("packetsReceived"),
    audioLevel: b.audioLevel,
    totalAudioEnergy: delta("totalAudioEnergy"),
    browserOutput: phase.output.at(-1),
    ...(sender === "native" ? {
      nativeFramesReceived: n.framesReceived - p.framesReceived,
      nativeFramesSent: n.framesSent - p.framesSent,
      nativeRms: Math.sqrt(nativeEnergy / nativeSamples),
      nativeToneFraction: nativeEnergy > 0 ? (n.toneEnergy - p.toneEnergy) / nativeEnergy : 0,
      nativeUnderruns: n.underruns - p.underruns,
      nativeUnderrunSamples: n.underrunSamples - p.underrunSamples,
    } : {}),
  };
}

async function main() {
  const started = Date.now();
  const nativeBin = path.resolve(repoRoot, process.env.E2E_AUDIO_FIXTURE ?? "target/debug/examples/web_audio");
  assert(nativeBin.startsWith(path.join(repoRoot, "target") + path.sep), "fixture must be built in this checkout's target directory");
  report.fixture = path.relative(repoRoot, nativeBin);
  assert(existsSync(nativeBin), "build the fixture first: cargo build -p gwar-client --example web_audio");
  report.fixtureSha256 = createHash("sha256").update(readFileSync(nativeBin)).digest("hex");
  report.harnessSha256 = createHash("sha256").update(readFileSync(fileURLToPath(import.meta.url))).digest("hex");
  const webUrl = `http://127.0.0.1:${await port()}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e"], {
    cwd: webRoot, env: { ...process.env, NODE_ENV: "development", VITE_CONNECT_URL: `${webUrl}/connect` }, encoding: "utf8",
  });
  writeFileSync(path.join(out, "build.log"), `${build.stdout}${build.stderr}`);
  assert.equal(build.status, 0, "e2e web build failed; see build.log");
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start("vite", process.execPath, [viteCli, "preview", "--host", "127.0.0.1", "--port", new URL(webUrl).port, "--strictPort"], { cwd: webRoot });
  await waitFor(async () => {
    try { return (await fetch(webUrl, { signal: AbortSignal.timeout(500) })).ok; } catch { return false; }
  }, "vite preview");
  const fixture = start("fixture", nativeBin, [webUrl, ...(process.env.E2E_RTP_TRACE === "1" ? ["--trace-rtp"] : [])]);
  const lines = createInterface({ input: fixture.stdout })[Symbol.asyncIterator]();
  async function response(event) {
    const line = await deadline(lines.next(), 15000, `fixture ${event}`);
    assert(!line.done, `fixture exited before ${event}`);
    const value = JSON.parse(line.value);
    assert.equal(value.event, event);
    return value;
  }
  async function command(cmd) {
    fixture.stdin.write(`${cmd}\n`);
    return response(cmd);
  }
  report.webUrl = webUrl;
  report.server = await response("ready");
  browser = await chromium.launch({ headless: true, args: [
    "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",
    `--use-file-for-fake-audio-capture=${microphoneWav()}`,
    "--autoplay-policy=no-user-gesture-required",
  ] });
  report.browserVersion = browser.version();
  const context = await browser.newContext({ locale: "en-US", permissions: ["microphone"] });
  await context.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.hostname !== "127.0.0.1") {
      report.errors.push(`blocked non-loopback request: ${url}`);
      return route.abort();
    }
    return route.continue();
  });
  await context.addInitScript(spy);
  const page = await context.newPage();
  page.on("pageerror", (error) => report.errors.push(error.message));
  page.on("websocket", (ws) => {
    assert.equal(new URL(ws.url()).hostname, "127.0.0.1", "non-loopback WebSocket");
    ws.on("framereceived", ({ payload }) => {
      try {
        const frame = JSON.parse(String(payload));
        if (frame.ev === "voice.slot") report.slotEvents.push({ ...frame.d, at: Date.now() });
      } catch { /* Ignore non-JSON transport frames. */ }
    });
  });
  await page.goto(webUrl);
  await connect(page, report.server.url);

  let sourceContext;
  let sourcePage;
  async function speakerCommand(cmd) {
    if (sender === "native") return command(cmd);
    if (["start", "replace"].includes(cmd)) {
      await sourceContext?.close();
      sourceContext = await browser.newContext({ locale: "en-US", permissions: ["microphone"] });
      await sourceContext.route("**/*", (route) => {
        const url = new URL(route.request().url());
        if (url.hostname !== "127.0.0.1") {
          report.errors.push(`blocked non-loopback sender request: ${url}`);
          return route.abort();
        }
        return route.continue();
      });
      await sourceContext.addInitScript(spy);
      sourcePage = await sourceContext.newPage();
      sourcePage.on("pageerror", (error) => report.errors.push(error.message));
      await sourcePage.goto(webUrl);
      await connect(sourcePage, report.server.url);
    } else if (["mute", "resume"].includes(cmd)) {
      await sourcePage.evaluate(async (muted) => {
        const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
        controller.setMuted(muted);
      }, cmd === "mute");
    } else if (cmd === "stop") {
      await sourceContext.close();
      return { event: cmd };
    }
    return sourcePage.evaluate(async (cmd) => {
      const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
      const pc = window.__pcs.at(-1);
      return { event: cmd, native: { session: useSession.getState().me.session, frequency: 700 }, offer: pc.localDescription.sdp, answer: pc.remoteDescription.sdp };
    }, cmd);
  }

  for (const [index, cmd] of ["start", "replace"].entries()) {
    let start;
    let slot;
    // The SFU chooses the oldest available slot, including slots never used.
    // Visit the other seven fixed slots before requiring the first to be reused.
    for (let attempt = 0; attempt < 8; attempt++) {
      if (index === 1 && scenario === "pause") {
        await speakerCommand("mute");
        await sleep(gapMs);
        start = await speakerCommand("resume");
      } else if (index === 1 && scenario === "continuous") {
        start = await speakerCommand("snapshot");
      } else if (index === 1 && scenario === "prewarmed" && attempt === 7) {
        await speakerCommand("prepare");
        await sleep(1000);
        start = await speakerCommand("activate");
        assert.equal(start.native.link, "Connected", "prepared sender ICE is not connected");
        assert.equal(start.native.framesSent, 0, "prepared sender transmitted before activation");
      } else {
        start = await speakerCommand(cmd);
      }
      await waitFor(() => {
        slot = report.slotEvents.findLast((event) => event.client === start.native.session)?.slot;
        return slot !== undefined;
      }, "native speaker slot");
      report.reassignments.push({ session: start.native.session, slot });
      if (cmd === "start" || slot === report.phases[0].slot) break;
      await sleep(150);
    }
    if (cmd === "replace") assert.equal(slot, report.phases[0].slot, "could not reuse the original receive slot");
    const phase = { frequency: start.native.frequency, session: start.native.session, nativeOffer: start.offer, nativeAnswer: start.answer, output: [], timeline: [] };
    report.phases.push(phase);
    await waitFor(() => {
      phase.slot = report.slotEvents.findLast((event) => event.client === phase.session)?.slot;
      return phase.slot !== undefined;
    }, "native speaker slot");
    const transitionStats = await stats(page, phase.slot);
    phase.transition = { at: Date.now(), browser: transitionStats, native: start.native };
    await waitFor(async () => {
      const tone = await outputTone(page, phase.frequency);
      return tone?.state === "running" && tone.rms > 0.05 && tone.toneFraction > 0.8;
    }, `${phase.frequency} Hz in browser speaker output`);
    phase.startup = [];
    for (let second = 0; second < 2; second++) {
      await sleep(1000);
      phase.startup.push(await stats(page, phase.slot));
    } // Keep startup separately from steady-state deltas.
    phase.before = await stats(page, phase.slot);
    phase.nativeBefore = (await speakerCommand("snapshot")).native;
    for (let second = 0; second < (index === 0 ? preSeconds : postSeconds); second++) {
      await sleep(1000);
      phase.output.push(await outputTone(page, phase.frequency));
      phase.timeline.push({ browser: await stats(page, phase.slot), native: (await speakerCommand("snapshot")).native });
    }
    phase.after = await stats(page, phase.slot);
    phase.nativeAfter = (await speakerCommand("snapshot")).native;
    phase.summary = summarize(phase);
    console.log(JSON.stringify(phase.summary));
  }
  const trace = await command("packets");
  report.rtpTrace = trace.packets;
  report.rtpTraceControlReplyErrors = trace.controlReplyErrors;
  // A negative control: a stopped sender must disappear from the app's output.
  await speakerCommand("stop");
  await waitFor(async () => (await outputTone(page, report.phases.at(-1).frequency))?.rms < 0.01, "silence after native leaves");
  fixture.stdin.end("quit\n");
  await deadline(once(fixture, "exit"), 15000, "fixture shutdown").then(([code]) => assert.equal(code, 0, "fixture shutdown"));
  const [first, second] = report.phases;
  assert.equal(first.slot, second.slot, "speaker change did not reuse a slot");
  if (["reuse", "prewarmed"].includes(scenario)) assert.notEqual(first.session, second.session, "speaker identity did not change");
  else assert.equal(first.session, second.session, "same-sender control changed identity");
  assert.equal(first.after.trackId, second.after.trackId, "slot receiver track changed");
  assert.equal(first.after.inbound.ssrc, second.after.inbound.ssrc, "slot SSRC changed");
  assert.equal(second.after.pcCount, 1, "browser renegotiated with a new peer connection");
  for (const phase of report.phases) {
    const s = phase.summary;
    assert.equal(phase.before.state, "connected");
    assert.equal(phase.after.state, "connected");
    assert.equal(phase.after.codec.mimeType.toLowerCase(), "audio/opus");
    assert(Math.abs(s.packetsPerSecond - 48000 / packetSamples) < 5, "unexpected sender packet rate");
    assert.equal(s.packetsLost, 0, "loopback packet loss");
    assert(s.concealmentPercent < 1, `browser concealment ${s.concealmentPercent}%`);
    assert(s.jitterBufferMs > 0 && (research || s.jitterBufferMs < 60), `browser jitter buffer ${s.jitterBufferMs} ms`);
    assert(Math.abs(s.samplesPerPacket - packetSamples) < 20, "decoded sample clock does not match packet duration");
    if (sender === "native") assert.equal(phase.nativeAfter.link, "Connected", "native transport lost");
    // Chromium measures these fields after the muted media-element output gain,
    // whereas the app's WebAudio path receives PCM before that gain. Keep both
    // stats in the report and require the tone at the actual app speaker output.
    assert(Number.isFinite(s.audioLevel) && Number.isFinite(s.totalAudioEnergy), "missing browser energy stats");
    for (const tone of phase.output) assert(tone.state === "running" && tone.rms > 0.05 && tone.toneFraction > 0.8, "browser did not play the expected native tone");
    if (sender === "native") {
      assert(s.nativeFramesReceived > s.seconds * 45, "browser -> native transport stalled");
      assert(s.nativeRms > 0.05 && s.nativeToneFraction > 0.8, "native did not play the browser's 700 Hz tone");
    }
  }
  assert.deepEqual(report.errors, [], "browser errors or non-loopback requests");
  report.elapsedSeconds = (Date.now() - started) / 1000;
  report.passed = true;
  console.log(`PASS ${sender} audio (${scenario}) in ${report.elapsedSeconds.toFixed(1)} s; ${out}`);
}

try {
  await main();
} catch (error) {
  report.failure = error.stack;
  console.error(error.stack);
  process.exitCode = 1;
} finally {
  await browser?.close();
  for (const { child } of children.toReversed()) await stop(child);
  for (const { name, log } of children) writeFileSync(path.join(out, `${name}.log`), log());
  report.wallSeconds = (Date.now() - runStarted) / 1000;
  writeFileSync(path.join(out, "report.json"), JSON.stringify(report, null, 2));
  console.log(`Evidence: ${out}`);
}
