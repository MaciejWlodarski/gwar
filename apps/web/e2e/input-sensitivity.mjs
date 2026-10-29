#!/usr/bin/env node
/** Built-app sensitivity checks and Polish approval screenshots, using generated audio only.
 * E2E_SCREENSHOTS selects the output directory. Only loopback URLs are permitted.
 */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { stopProcesses } from "./processes.mjs";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const output = path.resolve(process.env.E2E_SCREENSHOTS ?? path.join(webRoot, "e2e/screenshots/input-sensitivity"));
const scratch = mkdtempSync(path.join(tmpdir(), "gwar-input-sensitivity-e2e-"));
const children = [];
const screenshots = [];
let browser;
try {
  mkdirSync(output, { recursive: true });
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  const url = `http://127.0.0.1:${port}`;
  const dist = path.join(scratch, "dist");
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", dist], {
    cwd: webRoot, env: { ...process.env, VITE_CONNECT_URL: "http://127.0.0.1:1", NODE_ENV: "development" }, encoding: "utf8",
  });
  assert.equal(build.status, 0, `build failed:\n${build.stdout}${build.stderr}`);
  const vite = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  children.push(spawn(process.execPath, [vite, "preview", "--outDir", dist, "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: webRoot, stdio: "ignore", detached: true }));
  for (const started = Date.now(); ;) {
    try { if ((await fetch(url)).ok) break; } catch { /* Starting. */ }
    assert.ok(Date.now() - started < 30000, "local app did not start");
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  browser = await chromium.launch({ args: ["--autoplay-policy=no-user-gesture-required"] });
  const context = await browser.newContext({ locale: "pl-PL", reducedMotion: "reduce" });
  await context.route("**/*", route => {
    const request = new URL(route.request().url());
    return ["127.0.0.1", "localhost"].includes(request.hostname) ? route.continue() : route.abort();
  });
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", error => errors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem("gwar.settings", JSON.stringify({ version: 3, state: { language: "pl", theme: "dark", audio: { noiseSuppression: false, echoCancellation: false, autoGainControl: false } } }));
    // A real WebAudio signal feeds the real mic-test analyser; no level/store injection.
    let context, gain;
    window.setTestInputDb = db => { gain.gain.value = Math.SQRT2 * 10 ** (db / 20); };
    navigator.mediaDevices.getUserMedia = async () => {
      context ??= new AudioContext();
      await context.resume();
      const oscillator = context.createOscillator();
      gain = context.createGain();
      window.setTestInputDb(-70);
      oscillator.frequency.value = 440;
      const destination = context.createMediaStreamDestination();
      oscillator.connect(gain).connect(destination);
      oscillator.start();
      destination.stream.getAudioTracks()[0].addEventListener("ended", () => oscillator.stop());
      return destination.stream;
    };
  });
  const automatic = page.getByRole("switch", { name: /Automatycznie ustalaj czułość/ });
  const meter = page.getByRole("meter");
  for (const [name, width, height] of [["desktop", 1440, 1000], ["tablet", 768, 1024], ["mobile", 390, 844]]) {
    await page.setViewportSize({ width, height });
    // A fresh page per width avoids resizing an active mic test across a layout remount.
    await page.goto(url);
    await page.waitForFunction(() => typeof window.__gwarE2eImport === "function" && document.getElementById("root").childElementCount > 0);
    await page.evaluate(async () => {
      const { useUi } = await window.__gwarE2eImport("/src/state/stores.ts");
      useUi.getState().openSettings("audio");
      const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
      useSettings.getState().setAudio({ automaticSensitivity: true, vadThreshold: -50 });
    });
    await page.getByRole("button", { name: "Testuj mikrofon", exact: true }).click();
    await page.waitForFunction(() => {
      const meter = document.querySelector('[role="meter"]');
      return Number(meter?.getAttribute("aria-valuenow")) > 20 && Number(meter?.getAttribute("aria-valuenow")) < 40;
    });
    await automatic.scrollIntoViewIfNeeded();
    const shot = async state => {
      assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${name}: horizontal overflow`);
      const file = `${name}-${state}-pl.png`;
      await page.screenshot({ path: path.join(output, file) });
      screenshots.push(`${file}: ${width}x${height}, Polish, ${state}; generated input, threshold -50 dBFS.`);
    };
    await assert.doesNotReject(() => automatic.waitFor());
    await shot("auto-on");
    await automatic.click();
    const slider = page.getByRole("slider", { name: "Czułość mikrofonu", exact: true });
    await slider.scrollIntoViewIfNeeded();
    await page.waitForFunction(() => Number(document.querySelector('[role="meter"]').getAttribute("aria-valuenow")) < -60 && Number(document.querySelector('[role="meter"]').getAttribute("aria-valuenow")) > -80);
    await page.getByText("Poniżej progu · mikrofon pozostałby cichy", { exact: true }).waitFor();
    await shot("manual-below");
    await page.evaluate(() => window.setTestInputDb(-30));
    await page.waitForFunction(() => Number(document.querySelector('[role="meter"]').getAttribute("aria-valuenow")) > -40);
    await page.getByText("Powyżej progu · mikrofon nadawałby", { exact: true }).waitFor();
    await shot("manual-above");
    assert.equal(await slider.getAttribute("aria-valuenow"), "-50");
    assert.ok(Number(await meter.getAttribute("aria-valuenow")) > -40);
  }
  // Exercise the real worklet's output through WebAudio, including hangover and PTT.
  const levels = await page.evaluate(async () => {
    const { startInputGate } = await window.__gwarE2eImport("/src/voice/input-gate.ts");
    const ctx = new AudioContext();
    await ctx.resume();
    const oscillator = ctx.createOscillator(), gain = ctx.createGain(), capture = ctx.createMediaStreamDestination();
    oscillator.connect(gain).connect(capture);
    oscillator.start();
    const gate = await startInputGate(ctx, capture.stream);
    gate.setAudio(capture.stream.getAudioTracks()[0]);
    const analyser = ctx.createAnalyser();
    analyser.fftSize = 256;
    ctx.createMediaStreamSource(new MediaStream([gate.track])).connect(analyser);
    const pcm = new Float32Array(analyser.fftSize);
    const db = () => { analyser.getFloatTimeDomainData(pcm); return 10 * Math.log10(pcm.reduce((sum, v) => sum + v * v, 0) / pcm.length + 1e-20); };
    const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
    const input = db => { gain.gain.value = Math.SQRT2 * 10 ** (db / 20); };
    const controls = { threshold: -50, ptt: null, blocked: false };
    gate.configure(controls);
    input(-70); await wait(450); const below = db();
    input(-30); await wait(150); const above = db();
    input(-70); await wait(100); const tail = db();
    await wait(300); const closed = db();
    gate.configure({ ...controls, ptt: true }); await wait(150); const ptt = db();
    gate.configure({ ...controls, ptt: false }); await wait(150); const released = db();
    gate.configure({ ...controls, ptt: true, blocked: true }); await wait(150); const muted = db();
    gate.close(); oscillator.stop(); await ctx.close();
    return { below, above, tail, closed, ptt, released, muted };
  });
  assert.ok(levels.below < -110 && levels.closed < -110 && levels.released < -110 && levels.muted < -110, JSON.stringify(levels));
  assert.ok(levels.above > -35 && levels.tail > -75 && levels.ptt > -75, JSON.stringify(levels));
  assert.deepEqual(errors, []);
  writeFileSync(path.join(output, "screenshots.txt"), [
    `Captured ${new Date().toISOString()} from the built local web app (${url}).`,
    "No production services, real microphone, native app or user data were used.",
    "The meter is measured by the app from a generated 440 Hz sine: -70 dBFS below, -30 dBFS above.",
    "Automatic mode retains the default -50 dBFS gate. Manual screenshots use -50 dBFS.",
    ...screenshots, `Real AudioWorklet output levels (dBFS): ${JSON.stringify(levels)}`, "",
  ].join("\n"));
  console.log(JSON.stringify({ output, screenshots: screenshots.length, levels }));
} finally {
  await browser?.close();
  await stopProcesses(children);
  rmSync(scratch, { recursive: true, force: true });
}
