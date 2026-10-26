#!/usr/bin/env node
/**
 * Chromium real-tab startup regression. Fresh publishes of the sparse 20 fps barcode and a
 * busy 1920x1080 60 fps source; source, encoder AND viewer must recover after settling.
 * This intentionally does not use #166's fixed canvas: Chromium must adapt the real capture.
 * E2E_STARTUP_CODECS=vp9,h264 (default), E2E_STARTUP_REPEATS=2, E2E_STARTUP_SECONDS=60.
 * E2E_STARTUP_UPLINK=1500000 or 3000000 enables CDP's global p2p shaper and records tradeoffs
 * instead of requiring 1080p. All services and requests are local; no Connect/production.
 * E2E_STARTUP_PRESET=text|smooth|saver checks the other presets (default game).
 * E2E_STARTUP_DIST may select an independently built main bundle for before/after evidence.
 */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { stopProcesses } from "./processes.mjs";
import { canvasCapture, readBarcode, stampBarcode } from "./screen-source.mjs";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const SOURCE_TITLE = "Gwar startup source";
const children = [], opened = [];
let interrupted = false;
process.once("SIGINT", () => { interrupted = true; });
process.once("SIGTERM", () => { interrupted = true; });
const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-startup-"));
const resultDir = process.env.E2E_STARTUP_RESULTS ?? path.join(dataDir, "results");
mkdirSync(resultDir, { recursive: true });
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
  const started = performance.now();
  for (;;) {
    if (interrupted) throw new Error("startup regression interrupted");
    if (await check()) return performance.now() - started;
    if (performance.now() - started > timeout) throw new Error(`timed out: ${what}`);
    await sleep(150);
  }
}

/** Per device: a synthetic microphone, the barcode reader and, for canvas sharers, the capture hook. */
function devicePatch({ frequency, sources }, stamp, read, capture) {
  window.__readBarcode = read;
  // Every peer connection, for the RTP diagnostics a failure prints.
  window.__pcs = [];
  window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
    construct(target, args) { const pc = new target(...args); window.__pcs.push(pc); return pc; },
  });
  const media = navigator.mediaDevices;
  media.getUserMedia = async () => {
    const context = new AudioContext();
    const oscillator = context.createOscillator();
    oscillator.frequency.value = frequency;
    const destination = context.createMediaStreamDestination();
    oscillator.connect(destination);
    oscillator.start();
    void context.resume();
    return destination.stream;
  };
  media.enumerateDevices = async () => [];
  if (sources.length) capture(sources, stamp);
}

/** What a failure prints per device: each open peer connection's RTP streams and the stage's videos. */
const diagnostics = page => page.evaluate(async () => ({
  rtp: await Promise.all(window.__pcs.filter(pc => pc.connectionState !== "closed").map(async pc => {
    const stats = await pc.getStats();
    const codecs = {};
    const streams = [];
    stats.forEach(s => { if (s.type === "codec") codecs[s.id] = `${s.mimeType} ${s.sdpFmtpLine ?? ""}`.trim(); });
    stats.forEach(s => {
      if (s.type !== "inbound-rtp" && s.type !== "outbound-rtp") return;
      streams.push({ dir: s.type === "inbound-rtp" ? "in" : "out", mid: s.mid, codec: codecs[s.codecId], packets: s.packetsReceived ?? s.packetsSent, bytes: s.bytesReceived ?? s.bytesSent,
        frames: s.framesDecoded ?? s.framesEncoded, keyFrames: s.keyFramesDecoded ?? s.keyFramesEncoded, pli: s.pliCount, fir: s.firCount, nack: s.nackCount, size: `${s.frameWidth}x${s.frameHeight}` });
    });
    return { state: pc.connectionState, streams };
  })),
  videos: [...document.querySelectorAll("video[data-share]")].map(v => {
    const track = v.srcObject?.getVideoTracks?.()[0];
    return { share: v.dataset.share, readyState: v.readyState, size: `${v.videoWidth}x${v.videoHeight}`, paused: v.paused, track: track && `${track.readyState}${track.muted ? " muted" : ""}` };
  }),
}));


async function device(browser, web, server, name, { frequency = 440 } = {}) {
  const context = await browser.newContext({ locale: "en-US", viewport: { width: 1920, height: 1080 } });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  const sources = [];
  await context.addInitScript({ content: `(${devicePatch})(${JSON.stringify({ frequency, sources })}, ${stampBarcode}, ${readBarcode}, ${canvasCapture})` });
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
  await page.getByText("Not in voice", { exact: true }).waitFor({ timeout: 30_000 });
  page.deviceName = name;
  page.engine = browser.browserType().name();
  page.context_ = context;
  opened.push(page);
  return page;
}

/**
 * Runs `fn(session, screen, controller, arg)` in the page. The app's CSP forbids `new Function`,
 * so the function travels as the source of a string expression Playwright evaluates itself.
 */
const session = (page, fn, arg) => page.evaluate(`(async () => {
  const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { useScreen } = await window.__gwarE2eImport("/src/state/screen.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  return (${fn})(useSession.getState(), useScreen.getState(), controller, ${JSON.stringify(arg ?? null)});
})()`);

async function join(page, channel) {
  await waitFor(async () => {
    if (await session(page, (s, _, __, ch) => s.me !== null && s.clients[s.me.session]?.channel === ch, channel)) return true;
    await page.evaluate(async channel => {
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      await controller.joinChannel(channel).catch(() => {});
    }, channel);
    await sleep(400);
    return false;
  }, `${page.deviceName} joins channel ${channel}`, 40_000);
}

/** The share of the device named `nickname`, as `page` knows it. */
const ownShare = page => session(page, (_, screen) => (screen.own?.phase === "live" ? screen.own.share : null));
const bar = page => page.getByRole("status", { name: "You are sharing your screen" });

/** Shares through the UI: the Share button opens the picker, then the preset menu. */
async function share(page, surface) {
  await page.getByRole("button", { name: "Share screen", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Share your screen" });
  await dialog.waitFor({ timeout: 20_000 });
  await dialog.getByText(surface, { exact: true }).waitFor();
  await dialog.getByRole("radio", { name: preset === "text" ? "Sharp text" : "Smoothness", exact: true }).check();
  await dialog.getByLabel("Resolution", { exact: true }).selectOption(preset === "saver" ? "720" : "1080");
  await dialog.getByLabel("Frame rate", { exact: true }).selectOption(preset === "game" ? "60" : preset === "text" ? "15" : "30");
  await dialog.getByRole("button", { name: "Share", exact: true }).click();
  await bar(page).getByText(/watching$/).waitFor({ timeout: 30_000 });
  const id = await ownShare(page);
  assert.ok(id, `${page.deviceName} has a live share`);
  return id;
}
async function stopSharing(page) {
  await bar(page).getByRole("button", { name: "Stop sharing" }).click();
  await bar(page).waitFor({ state: "detached" });
}

/** Opens `sharer`'s share from the channel tree's LIVE badge. */
async function watch(page, sharer) {
  await page.getByRole("button", { name: `${sharer} is sharing their screen. Click to watch.` }).click();
}

// Large moving blocks across the full frame, calibrated to fit Game's 8 Mb/s cap
// in both codecs. Fine-grained noise/text can legitimately quality-scale in H.264.
function busySource(canvas) {
  const ctx = canvas.getContext("2d");
  let f = 0;
  function draw() {
    f++; window.__drawnFrames = f;
    ctx.fillStyle = "#17212e"; ctx.fillRect(0, 0, 1920, 1080);
    for (let y=0; y<9; y++) for(let x=0; x<16; x++) {
      ctx.fillStyle = `hsl(${(x*19+y*11+f*2)%360} 60% ${30+(x+y)%25}%)`;
      const dx = Math.sin(f/17+y)*18, dy=Math.cos(f/23+x)*12;
      ctx.fillRect(x*120+dx, y*120+dy, 90, 84);

    }
    ctx.fillStyle="white";ctx.font="32px sans-serif";ctx.fillText(`1080p60 frame ${f}`,50,1040);
  }
  draw(); setInterval(draw, 1000/60);
}

const median = xs => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];
const results = [], failures = [];
const seconds = Number(process.env.E2E_STARTUP_SECONDS ?? 60);
const uplink = Number(process.env.E2E_STARTUP_UPLINK ?? 0);
const preset = process.env.E2E_STARTUP_PRESET ?? "game";
const caps = { game: [1920, 1080, 60], text: [1920, 1080, 15], smooth: [1920, 1080, 30], saver: [1280, 720, 30] };
assert.ok(caps[preset], `unknown preset: ${preset}`);
const [targetWidth, targetHeight, maxFps] = caps[preset];
assert.ok(seconds >= 25, "allow at least ten seconds of settled observations");
let browser, server;
try {
  const dist = process.env.E2E_STARTUP_DIST ?? path.join(dataDir, "dist");
  if (!process.env.E2E_STARTUP_DIST) {
    const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", dist], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
    assert.equal(build.status, 0, `e2e build: ${build.stdout}${build.stderr}`);
  }
  const webPort = await port(), serverPort = await port();
  const web = `http://127.0.0.1:${webPort}`, ws = `ws://127.0.0.1:${serverPort}/ws`;
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start("vite", process.execPath, [viteCli, "preview", "--outDir", dist, "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web");
  server = start("server", process.env.GWAR_SERVER_BIN ?? path.join(repoRoot, "target/debug/gwar-server"), ["--data-dir", path.join(dataDir, "server"), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"]);
  await waitFor(() => /redeem this one-time admin token in your client: (\S+)/.test(server.output), "server", 60_000);
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)[1];
  browser = await chromium.launch({ channel: "chromium", headless: true, args: ["--autoplay-policy=no-user-gesture-required", `--auto-select-tab-capture-source-by-title=${SOURCE_TITLE}`, "--disable-background-timer-throttling", "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows"] });
  writeFileSync(path.join(resultDir, "metadata.json"), JSON.stringify({ browser: browser.version(), platform: process.platform, seconds, uplink, dist }, null, 2));
  const admin = await device(browser, web, ws, "startup-admin");
  const channel = await admin.evaluate(async token => {
    const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
    await controller.connection.request("token.redeem", { token });
    return (await controller.createChannel({ name: "Startup", encrypted: false })).id;
  }, token);
  for (const codec of (process.env.E2E_STARTUP_CODECS ?? "vp9,h264").split(",")) {
    for (const kind of (process.env.E2E_STARTUP_SOURCES ?? "barcode20,busy60").split(",")) {
      for (let repeat = 0; repeat < Number(process.env.E2E_STARTUP_REPEATS ?? 2); repeat++) {
        const name = `${codec}-${kind}-${repeat}`, rows = [];
        const sharer = await device(browser, web, ws, name);
        const supported = await sharer.evaluate(codec => RTCRtpSender.getCapabilities("video").codecs.some(c => c.mimeType.toLowerCase() === `video/${codec}`), codec);
        if (!supported) throw new Error(`${codec} encoder unavailable; this is not a passing quality run`);
        await sharer.evaluate(codec => {
          const original = RTCRtpTransceiver.prototype.setCodecPreferences;
          RTCRtpTransceiver.prototype.setCodecPreferences = function(codecs) {
            return original.call(this, codecs.filter(c => ["video/rtx", `video/${codec}`].includes(c.mimeType.toLowerCase())));
          };
        }, codec);
        if (uplink) {
          const cdp = await sharer.context_.newCDPSession(sharer);
          // Empty URL pattern explicitly includes WebRTC/p2p, unlike HTTP-only route throttling.
          await cdp.send("Network.emulateNetworkConditionsByRule", { offline: false, matchedNetworkConditions: [{ urlPattern: "", latency: 20, downloadThroughput: -1, uploadThroughput: uplink / 8, packetQueueLength: 50 }] });
        }
        const source = await sharer.context_.newPage();
        const draw = kind === "busy60" ? `(${busySource})(document.querySelector('canvas'))` : `(${stampBarcode})(document.querySelector('canvas'), 1); window.__drawnFrames = 0; setInterval(() => window.__drawnFrames++, 50)`;
        await source.setContent(`<!doctype html><title>${SOURCE_TITLE}</title><style>html,body{margin:0;overflow:hidden}canvas{display:block;width:1920px;height:1080px}</style><canvas width="1920" height="1080"></canvas><script>${draw}</script>`);
        await sharer.bringToFront(); await join(sharer, channel);
        const started = performance.now(), wallStarted = Date.now();
        const id = await share(sharer, "Sharing a browser tab");
        await watch(admin, name);
        for (;;) {
          if (interrupted) throw new Error("startup regression interrupted");
          const row = await sharer.evaluate(async () => {
            for (const pc of window.__pcs) {
              if (pc.connectionState === "closed") continue;
              const report = await pc.getStats(), stats = [...report.values()];
              const out = stats.find(s => s.type === "outbound-rtp" && s.kind === "video");
              if (!out) continue;
              const transport = report.get(out.transportId), pair = report.get(transport?.selectedCandidatePairId);
              const sender = pc.getSenders().find(s => s.track?.kind === "video");
              return { out, source: report.get(out.mediaSourceId), pair, codec: report.get(out.codecId)?.mimeType, params: sender.getParameters(), track: sender.track.getSettings() };
            }
            return {};
          });
          row.elapsed = (performance.now() - started) / 1000;
          row.wallElapsed = (Date.now() - wallStarted) / 1000;
          row.drawn = await source.evaluate(() => window.__drawnFrames);
          row.viewer = await session(admin, (_, __, controller, id) => controller.screen.stats(id), id);
          rows.push(row);
          appendFileSync(path.join(resultDir, `${name}.jsonl`), JSON.stringify(row) + "\n");
          if (row.elapsed >= seconds) break;
          await sleep(500);
        }
        try {
          assert.ok(rows.every(r => Math.abs(r.wallElapsed - r.elapsed) < 2), "uninterrupted clock: suspend/wall-clock change invalidates timing");
          const settled = rows.filter(r => r.elapsed >= seconds - 10);
          assert.ok(settled.length >= 15, "ten seconds of observations");
          assert.ok(settled.every(r => r.viewer), "viewer stats throughout the settled window");
          const first = settled[0], last = settled.at(-1), dt = last.elapsed - first.elapsed;
          const cadence = (last.drawn - first.drawn) / dt;
          const payload = (last.out.bytesSent - first.out.bytesSent) * 8 / dt;
          const decodeFps = (last.viewer.framesDecoded - first.viewer.framesDecoded) / dt;
          const drawnFps = kind === "busy60" ? 60 : 20;
          const expectedFps = Math.min(drawnFps, maxFps);
          assert.ok(cadence >= drawnFps * 0.9 && cadence <= drawnFps * 1.12, `source cadence ${cadence}`);
          assert.ok(settled.every(r => r.codec?.toLowerCase() === `video/${codec}`), "actual negotiated codec");
          const full = r => r.out?.frameWidth === targetWidth && r.out?.frameHeight === targetHeight && r.source?.width === targetWidth && r.source?.height === targetHeight && r.viewer?.width === targetWidth && r.viewer?.height === targetHeight;
          const settledFps = median(settled.map(r => r.out.framesPerSecond));
          const stableFrame = r => uplink
            ? r.out?.frameWidth === last.out.frameWidth && r.out?.frameHeight === last.out.frameHeight && r.out?.framesPerSecond >= Math.max(1, settledFps * 0.8)
            : full(r) && r.out?.framesPerSecond >= expectedFps * 0.85;
          const stable = rows.find((r, i) => r.elapsed <= seconds - 5 && stableFrame(r) && rows.slice(i).every(stableFrame));
          const stableDimensions = rows.find((r, i) => r.elapsed <= seconds - 5 && rows.slice(i).every(s =>
            s.out?.frameWidth === last.out.frameWidth && s.out?.frameHeight === last.out.frameHeight));
          const firstTarget = rows.find(full);
          const steadyFps = rows.find((r, i) => r.elapsed <= seconds - 5 && rows.slice(i).every(s => s.out?.framesPerSecond >= expectedFps * 0.85));
          const firstFiveFps = median(rows.filter(r => r.elapsed <= 5 && r.out?.framesPerSecond !== undefined).map(r => r.out.framesPerSecond));
          const result = { name, preset, uplink, sourceFps: +cadence.toFixed(1), captureFps: median(settled.map(r => r.source?.framesPerSecond ?? 0)), output: `${last.out.frameWidth}x${last.out.frameHeight}`, source: `${last.source?.width}x${last.source?.height}`, viewer: `${last.viewer?.width}x${last.viewer?.height}`, fps: median(settled.map(r => r.out.framesPerSecond)), decodeFps: +decodeFps.toFixed(1), payloadMbps: +(payload / 1e6).toFixed(3), availableMbps: +(median(settled.map(r => r.pair.availableOutgoingBitrate)) / 1e6).toFixed(3), stableAt: stable?.elapsed ?? null, stableDimensionsAt: stableDimensions?.elapsed ?? null, firstTargetAt: firstTarget?.elapsed ?? null, steadyFpsAt: steadyFps?.elapsed ?? null, firstFiveFps, pulses: rows.filter((r,i) => r.elapsed > 4 && r.params.degradationPreference === "maintain-resolution" && rows[i-1]?.params.degradationPreference === "maintain-framerate").length };
          results.push(result); console.log(JSON.stringify(result));
          if (!uplink) {
            assert.ok(result.captureFps >= expectedFps * 0.85 && result.captureFps <= expectedFps * 1.12, `captured source cadence ${result.captureFps}`);
            assert.ok(result.fps >= expectedFps * 0.85, `encoded cadence ${result.fps}`);
            assert.ok(settled.every(full), `${name}: source, encoder and viewer must recover to ${targetWidth}x${targetHeight}`);
            assert.ok(settled.every(r => r.params.degradationPreference === ({ game: "maintain-framerate", text: "maintain-resolution", smooth: "maintain-framerate", saver: "maintain-framerate" })[preset]), "preset restored");
            assert.ok(decodeFps >= expectedFps * 0.75, `viewer cadence ${decodeFps}`);
          } else {
            assert.ok(payload <= uplink * 1.15 && payload > uplink * 0.15, `active shaped payload ${payload}`);
            assert.ok(result.availableMbps * 1e6 < uplink * 1.5, "shaper must affect WebRTC, not just HTTP");
            assert.ok(result.fps >= expectedFps * 0.85 && decodeFps >= expectedFps * 0.75, `active shaped encode/decode cadence ${result.fps}/${decodeFps}`);
            assert.ok(Math.abs(last.viewer.width - last.out.frameWidth) <= 2 && Math.abs(last.viewer.height - last.out.frameHeight) <= 2, "viewer decodes the shaped output (codec crop tolerance)");
            assert.equal(result.pulses, 0, "no nudge on a genuinely constrained uplink");
          }
        } catch (error) { failures.push(`${name}: ${error.message}`); console.error(JSON.stringify({ sharer: await diagnostics(sharer), viewer: await diagnostics(admin) })); }
        await session(admin, (_, __, controller, id) => controller.screen.unwatch(id), id);
        await stopSharing(sharer); await sharer.context_.close();
      }
    }
  }
  writeFileSync(path.join(resultDir, "summary.json"), JSON.stringify({ results, failures }, null, 2));
  assert.deepEqual(failures, []);
} catch (error) {
  console.error(error.stack ?? error); process.exitCode = 1;
  if (server?.exitCode) console.error(server.output.replace(/admin token in your client: \S+/g, "admin token: [redacted]"));
} finally {
  if (browser) await browser.close().catch(() => {});
  await stopProcesses(children);
  rmSync(dataDir, { recursive: true, force: true });
}
