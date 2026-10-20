#!/usr/bin/env node
/**
 * The web app's noise suppression in real browsers: the built app downloads the model, the
 * AudioWorklet loads it, and white noise sent through comes out far down; while the microphone
 * is not sent, it passes as captured. Needs crates/gwar-denoise-wasm/pkg (scripts/denoise-wasm/build.sh).
 *
 *   E2E_BROWSERS=chromium,webkit,firefox   engines to run (default: chromium,webkit)
 */
import { stopProcesses } from "./processes.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox, webkit } from "playwright";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const engines = (process.env.E2E_BROWSERS ?? "chromium,webkit").split(",").map(s => s.trim()).filter(Boolean);
const children = [];
const results = [];

async function port() {
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  return port;
}

/** In the page: noise at -30 dBFS through the model; the level that comes out, active and not. */
async function measure() {
  const { startDenoise } = await window.__gwarE2eImport("/src/voice/denoise/index.ts");
  const troubles = [];
  const started = performance.now();
  const model = await startDenoise({ trouble: reason => troubles.push(reason), changed: () => {} });
  const loadMs = performance.now() - started;
  const context = new AudioContext({ sampleRate: 48_000 });
  await context.resume();
  const noise = context.createBuffer(1, 48_000 * 2, 48_000);
  const samples = noise.getChannelData(0);
  for (let i = 0; i < samples.length; i++) samples[i] = (Math.random() - 0.5) * 2 * Math.sqrt(3) * 10 ** (-30 / 20);
  const source = context.createBufferSource();
  source.buffer = noise;
  source.loop = true;
  const microphone = context.createMediaStreamDestination();
  source.connect(microphone);
  source.start();
  model.setSource(microphone.stream);
  const analyser = context.createAnalyser();
  analyser.fftSize = 8192;
  context.createMediaStreamSource(new MediaStream([model.track])).connect(analyser);
  const pcm = new Float32Array(analyser.fftSize);
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const level = async () => {
    let sum = 0, n = 0;
    for (let k = 0; k < 10; k++) {
      await sleep(100);
      analyser.getFloatTimeDomainData(pcm);
      for (const s of pcm) { sum += s * s; n++; }
    }
    return 10 * Math.log10(sum / n + 1e-12);
  };
  model.setActive(false);
  await sleep(500);
  const passed = await level();
  model.setActive(true);
  await sleep(1500);
  const cleaned = await level();
  model.close();
  await context.close();
  return { loadMs, passed, cleaned, troubles };
}

async function run(engine, web) {
  const launchers = { chromium, firefox, webkit };
  const options = {
    chromium: { args: ["--autoplay-policy=no-user-gesture-required"] },
    firefox: { firefoxUserPrefs: { "media.autoplay.default": 0, "media.autoplay.block-webaudio": false } },
  };
  const browser = await launchers[engine].launch(options[engine] ?? {});
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", e => errors.push(String(e)));
    await page.goto(web);
    await page.waitForFunction(() => typeof window.__gwarE2eImport === "function");
    // A gesture, for browsers that start audio only after one.
    await page.mouse.click(1, 1);
    const result = await page.evaluate(measure);
    assert.deepEqual(result.troubles, [], "the model reported trouble");
    assert.ok(result.passed > -35, `inactive: passes as captured (${result.passed.toFixed(1)} dBFS)`);
    assert.ok(result.cleaned < result.passed - 20, `active: noise down by more than 20 dB (${result.passed.toFixed(1)} → ${result.cleaned.toFixed(1)} dBFS)`);
    results.push({ engine, status: "pass", ...result, errors });
  } finally { await browser.close(); }
}

const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-denoise-e2e-"));
try {
  const webPort = await port();
  const web = `http://127.0.0.1:${webPort}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", path.join(dataDir, "dist")], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, `e2e web build failed:\n${build.stdout}${build.stderr}`);
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  const preview = spawn(process.execPath, [viteCli, "preview", "--outDir", path.join(dataDir, "dist"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot, stdio: "ignore", detached: true });
  children.push(preview);
  for (let started = Date.now(); ; await new Promise(resolve => setTimeout(resolve, 100))) {
    try { if ((await fetch(web)).ok) break; } catch { /* not up yet */ }
    if (Date.now() - started > 30_000) throw new Error("timed out: web preview");
  }
  for (const engine of engines) {
    try { await run(engine, web); }
    catch (error) { results.push({ engine, status: "fail", error: String(error.stack ?? error) }); }
  }
} finally {
  await stopProcesses(children);
  rmSync(dataDir, { recursive: true, force: true });
}
for (const r of results) console.log(JSON.stringify(r));
if (results.some(r => r.status === "fail")) process.exit(1);
