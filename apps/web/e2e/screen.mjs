#!/usr/bin/env node
/**
 * Screen sharing in real browsers (docs/screen-share.md, PR 4): a real local gwar-server, the
 * built app, plaintext shares through the SFU.
 *
 * - A Chromium sharer captures a real tab: the full Chromium build in headless mode
 *   (`channel: "chromium"`) picks the tab titled `SOURCE_TITLE` itself
 *   (`--auto-select-tab-capture-source-by-title`), so the app's own `getDisplayMedia` call runs.
 * - The other sharers (WebKit, and extra Chromium ones) get a canvas track through the test hook:
 *   an init script standing in for `getDisplayMedia`.
 * - Every source stamps a barcode (source id, frame counter, check) into its frames; viewers in
 *   both engines read it back from the decoded video, so frames are checked end to end. (WebKit on
 *   Linux paints remote video unreliably; see `paintsUnreliably`.)
 *
 * Covered: Chromium and WebKit sharing to Chromium and WebKit viewers; a viewer in another
 * channel's voice and one in no voice at all; a late joiner who learns of shares from the
 * Welcome; start → stop → start while watching two other shares; the slot limit; a moderator
 * stop; and the Share button refusing in an encrypted channel.
 *
 *   E2E_BROWSERS=chromium,webkit   engines to use (default: both); with one, it plays every role
 *   E2E_SCREEN_CANVAS=1          fixed 1080p60 canvas instead of adaptive tab capture
 *   E2E_SCREEN_REQUIRE_H264=1    fail if the main sharer exposes no H.264 encoder
 *   E2E_SCREEN_TRACE_DIR        retain server logs, browser events/state and watchdog samples
 *   RUST_LOG=info,gwar_server::media::screen=debug  include SFU recovery decisions in the logs
 *   GWAR_SERVER_BIN                 server binary (default: target/debug/gwar-server)
 */
import { stopProcesses } from "./processes.mjs";
import { canvasCapture, readBarcode, stampBarcode } from "./screen-source.mjs";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const engines = (process.env.E2E_BROWSERS ?? "chromium,webkit").split(",").map(s => s.trim()).filter(Boolean);
// Keep codec/dimension admission deterministic: real-tab capture also drops to 480x270
// on main's VP9 path with Chromium reporting bandwidth pressure and zero CPU limitation.
// That separate capture/BWE investigation must not weaken this fixed-canvas H.264 gate.
const fixedCanvas = process.env.E2E_SCREEN_CANVAS === "1";
const traceDir = process.env.E2E_SCREEN_TRACE_DIR;
const SOURCE_TITLE = "Gwar share source";
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
  child.taskName = name;
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

/** Per device: a synthetic microphone, the barcode reader and, for canvas sharers, the capture hook. */
function devicePatch({ frequency, sources, sendCodec, receiveCodec, trace }, stamp, read, capture) {
  window.__screenTrace = [];
  if (trace) {
    const record = (type, data) => window.__screenTrace.push({ at: Date.now(), type, ...data });
    window.__traceScreen = record;
    window.WebSocket = new Proxy(window.WebSocket, {
      construct(target, args) {
        const socket = new target(...args);
        const inspect = (direction, data) => {
          try {
            const frame = JSON.parse(data);
            if ((frame.ev ?? frame.op ?? "").startsWith("screen.")) record(direction, { frame });
          } catch { /* Not a JSON screen frame. */ }
        };
        socket.addEventListener("message", event => inspect("event", event.data));
        const send = socket.send.bind(socket);
        socket.send = data => { inspect("request", data); return send(data); };
        return socket;
      },
    });
  }
  window.__readBarcode = read;
  // Every peer connection, for the RTP diagnostics a failure prints.
  window.__pcs = [];
  window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
    construct(target, args) { const pc = new target(...args); window.__pcs.push(pc); return pc; },
  });
  for (const [type, selected] of [[RTCRtpSender, sendCodec], [RTCRtpReceiver, receiveCodec]]) {
    if (!selected) continue;
    const capabilities = type.getCapabilities.bind(type);
    type.getCapabilities = kind => {
      const caps = capabilities(kind);
      return kind === "video" && caps ? { ...caps, codecs: caps.codecs.filter(c => ["video/rtx", `video/${selected}`].includes(c.mimeType.toLowerCase())) } : caps;
    };
  }
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

const sourcePage = `<!doctype html><title>${SOURCE_TITLE}</title><style>html,body{margin:0;height:100%;overflow:hidden;background:#777}canvas{display:block;width:100vw;height:100vh}</style>
<canvas width="1920" height="1080"></canvas><script>(${stampBarcode})(document.querySelector("canvas"), 1, 60)</script>`;

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
        fps: s.framesPerSecond, limitation: s.qualityLimitationReason, encoder: s.encoderImplementation, frames: s.framesDecoded ?? s.framesEncoded, keyFrames: s.keyFramesDecoded ?? s.keyFramesEncoded, pli: s.pliCount, fir: s.firCount, nack: s.nackCount, size: `${s.frameWidth}x${s.frameHeight}` });
    });
    return { state: pc.connectionState, streams, offeredCodecs: pc.localDescription?.sdp?.split(/\r?\n/).filter(l => /^m=video|^a=rtpmap:.*(H264|VP9|VP8)/i.test(l)), answeredCodecs: pc.remoteDescription?.sdp?.split(/\r?\n/).filter(l => /^m=video|^a=rtpmap:.*(H264|VP9|VP8)/i.test(l)) };
  })),
  capture: window.__pcs.flatMap(pc => pc.getSenders().filter(s => s.track?.kind === "video").map(s => ({ settings: s.track.getSettings(), parameters: s.getParameters() }))),
  videos: [...document.querySelectorAll("video[data-share]")].map(v => {
    const track = v.srcObject?.getVideoTracks?.()[0];
    return { share: v.dataset.share, readyState: v.readyState, size: `${v.videoWidth}x${v.videoHeight}`, paused: v.paused, track: track && `${track.readyState}${track.muted ? " muted" : ""}` };
  }),
}));

const errors = page => page.errors.filter(e => !/Conversation synchronization|favicon|Failed to load resource|Screen capture constraints|WebSocket/.test(e));

async function device(browser, web, server, name, { canvasId = null, frequency = 440, sendCodec = null, receiveCodec = null } = {}) {
  const context = await browser.newContext({ locale: "en-US", viewport: { width: 1920, height: 1080 } });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  const sources = (canvasId === null ? [] : [canvasId].flat()).map(id => ({ id, size: SOURCE_SIZES[id], ...(id === 1 && fixedCanvas ? { fps: 60 } : {}) }));
  await context.addInitScript({ content: `(${devicePatch})(${JSON.stringify({ frequency, sources, sendCodec, receiveCodec, trace: !!traceDir })}, ${stampBarcode}, ${readBarcode}, ${canvasCapture})` });
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
  if (traceDir) await page.evaluate(async () => {
    const { useScreen } = await window.__gwarE2eImport("/src/state/screen.ts");
    useScreen.subscribe(screen => window.__traceScreen("state", { transport: screen.transport, watches: screen.watches, slots: screen.slots }));
  });
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
const shareOf = (page, nickname) => session(page, (s, _, __, nick) => {
  const client = Object.values(s.clients).find(c => c.nickname === nick);
  return Object.values(s.shares).find(x => x.client === client?.id)?.share ?? null;
}, nickname);
const ownShare = page => session(page, (_, screen) => (screen.own?.phase === "live" ? screen.own.share : null));
const bar = page => page.getByRole("status", { name: "You are sharing your screen" });

/** Shares through the UI: the Share button opens the picker, then the preset menu. */
async function share(page, surface, preset = null) {
  await page.getByRole("button", { name: "Share screen", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Share your screen" });
  await dialog.waitFor({ timeout: 20_000 });
  // Synthetic canvas tracks may report either an unknown surface or a monitor.
  await dialog.getByText(surface === "Sharing your screen" ? /^Sharing your (?:entire )?screen$/ : surface, { exact: true }).waitFor();
  if (preset) {
    await dialog.getByRole("radio", { name: preset === "Text" ? "Sharp text" : "Smoothness", exact: true }).check();
    await dialog.getByLabel("Resolution", { exact: true }).selectOption(preset === "Saver" ? "720" : "1080");
    await dialog.getByLabel("Frame rate", { exact: true }).selectOption(preset === "Game" ? "60" : preset === "Text" ? "15" : "30");
  }
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

/**
 * WebKit on Linux (Playwright's GTK/WPE build, GStreamer media) paints remote video unreliably: a
 * `<video>` often stays at HAVE_NOTHING while its receiver keeps decoding, mostly when the element
 * gets the stream after frames already flow (reproduced without Gwar, two peer connections in one
 * page). Safari on macOS paints every one, and the macOS runs read the barcode in both engines. On
 * Linux a WebKit viewer checks the receiver's decoded frames instead.
 */
const paintsUnreliably = page => page.engine === "webkit" && process.platform === "linux";

/**
 * Each source's frame size, all of different shapes: a decoded frame's aspect ratio names the
 * source even where no pixel can be read (and survives the encoder scaling it down). Source 1 is
 * the Chromium tab, whose page has the context's 1920×1080 viewport, or a canvas of that size;
 * source 6 is the restarting sharer's second share.
 */
const SOURCE_SIZES = { 1: [1920, 1080], 2: [640, 400], 3: [640, 480], 4: [480, 480], 5: [600, 400], 6: [560, 400] };
function fromSource(id, width, height, what) {
  const [w, h] = SOURCE_SIZES[id];
  assert.ok(Math.abs(width / height - w / h) < 0.03, `${what}: a ${width}x${height} frame is not source ${id}'s ${w}x${h}`);
}

const revealed = (page, share) => waitFor(() => page.evaluate(share => {
  const video = document.querySelector(`video[data-share="${share}"]`);
  return video && video.readyState >= HTMLMediaElement.HAVE_CURRENT_DATA && video.videoWidth > 0 && video.videoHeight > 0 && getComputedStyle(video).opacity === "1";
}, share), "the new watch reveals its decoded frame");

/** Waits until `page` decodes `share` with the source's barcode, twice with different counters. */
async function sees(page, share, id, what) {
  if (paintsUnreliably(page)) return decodes(page, share, id, what);
  const read = () => page.evaluate(share => window.__readBarcode(document.querySelector(`video[data-share="${share}"]`)), share);
  let first = null, last = null;
  const ms = await waitFor(async () => {
    last = await read();
    if (!last || last.invalid || last.id !== id) return false;
    if (!first) { first = last; return false; }
    return last.counter !== first.counter;
  }, `${what}: ${page.deviceName} decodes source ${id} (last ${JSON.stringify(last)})`, 30_000);
  fromSource(id, last.width, last.height, what);
  const stats = await session(page, (_, __, controller, share) => controller.screen.stats(share), share);
  assert.ok(stats && stats.framesDecoded > 0, `${what}: frames decoded`);
  fromSource(id, stats.width, stats.height, `${what} (receiver)`);
  await revealed(page, share);
  checks.push({ check: what, viewer: page.deviceName, ms, size: `${last.width}x${last.height}`, framesDecoded: stats.framesDecoded });
}

/**
 * The receiver of `share`'s slot keeps decoding frames of source `id`'s shape: the check where the
 * page paints no video. The slot is the one the app holds for `share`, so a wrong slot mapping
 * shows as another source's shape. Without a painted frame no pixel can be read: WebKit has no
 * `MediaStreamTrackProcessor`, and `VideoFrame` and `requestVideoFrameCallback` need the painted
 * video.
 */
async function decodes(page, share, id, what) {
  const stats = () => session(page, (_, __, controller, share) => controller.screen.stats(share), share);
  let first = null, last = null;
  const ms = await waitFor(async () => {
    last = await stats();
    if (!last || last.framesDecoded === 0 || !last.width) return false;
    if (!first) { first = last; return false; }
    return last.framesDecoded >= first.framesDecoded + 5;
  }, `${what}: ${page.deviceName} keeps decoding (last ${JSON.stringify(last)})`, 30_000);
  fromSource(id, last.width, last.height, what);
  await revealed(page, share);
  checks.push({ check: what, viewer: page.deviceName, ms, size: `${last.width}x${last.height}`, framesDecoded: last.framesDecoded, painted: false });
}

/** getStats codec evidence from the actual publish and receive transports, not SDP alone. */
async function codecFlow(publisher, viewer, share, expected, size = null) {
  const slot = await session(viewer, (_, screen, __, share) => screen.watches.find(w => w.share === share)?.slot, share);
  const read = async (page, direction) => page.evaluate(async ({ direction, slot }) => {
    for (const pc of window.__pcs) {
      if (pc.connectionState !== "connected") continue;
      const transceivers = pc.getTransceivers();
      const target = direction === "in" ? transceivers[slot] : null;
      if (direction === "in" && (!target || target.direction !== "recvonly" || target.receiver.track.kind !== "video")) continue;
      const report = direction === "in" ? await target.receiver.getStats() : await pc.getStats();
      for (const s of report.values()) {
        if (s.type === (direction === "in" ? "inbound-rtp" : "outbound-rtp") && s.kind === "video" && (s.framesDecoded ?? s.framesEncoded) > 0)
          return { mimeType: report.get(s.codecId)?.mimeType?.toLowerCase(), frames: s.framesDecoded ?? s.framesEncoded, encoder: s.encoderImplementation ?? null,
            frameWidth: s.frameWidth, frameHeight: s.frameHeight, fps: s.framesPerSecond,
            fmtp: report.get(s.codecId)?.sdpFmtpLine,
            offeredFmtp: pc.localDescription?.sdp?.split(/\r?\n/).filter(l => l.startsWith("a=fmtp:")),
            answeredFmtp: pc.remoteDescription?.sdp?.split(/\r?\n/).filter(l => l.startsWith("a=fmtp:")) };
      }
    }
    return null;
  }, { direction, slot });
  let sent, received;
  await waitFor(async () => { sent = await read(publisher, "out"); received = await read(viewer, "in"); return sent?.mimeType === expected && received?.mimeType === expected &&
    (!size || [sent, received].every(s => s.frameWidth === size[0] && s.frameHeight === size[1])) &&
    (!(expected === "video/h264" && size?.[0] === 1920 && viewer.engine === "chromium") || sent.fps >= 50 && received.fps >= 50); }, `${expected} flows through the real SFU${size ? ` at ${size.join("x")}` : ""}`);
  if (expected === "video/h264" && size) {
    assert.ok(sent.answeredFmtp.some(l => /level-asymmetry-allowed=1/.test(l) && /profile-level-id=(42e0|640c|6400)33/.test(l)), "publisher answer retains receive level 5.1");
    assert.ok(received.offeredFmtp.some(l => /profile-level-id=(42e0|640c|6400)1f/.test(l)), "viewer starts with the browser's default level 3.1");
  }
  checks.push({ check: "codec flow through SFU", share, viewer: viewer.deviceName, sent, received });
}

async function run(browsers, web, server, token) {
  const engine = name => browsers[engines.includes(name) ? name : engines[0]];
  const chromiumTab = engines.includes("chromium") && !fixedCanvas;

  // A moderator viewer, in another channel's voice.
  const cv = await device(engine("chromium"), web, server, "cv-mod", { frequency: 300 });
  await cv.evaluate(async token => {
    const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
    await controller.connection.request("token.redeem", { token });
  }, token);
  const channels = await cv.evaluate(async () => {
    const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
    const stage = (await controller.createChannel({ name: "Stage", encrypted: false })).id;
    const hall = (await controller.createChannel({ name: "Hall", encrypted: false })).id;
    const secret = (await controller.createChannel({ name: "Secret", encrypted: true, history: true })).id;
    return { stage, hall, secret };
  });

  // Chromium shares a real tab; WebKit shares a canvas.
  const cs = await device(engine("chromium"), web, server, "cs-tab", { frequency: 440, canvasId: chromiumTab ? null : 1 });
  if (chromiumTab) {
    await (await cs.context_.newPage()).setContent(sourcePage);
    // Chromium refuses the picker to a tab in the background.
    await cs.bringToFront();
  }
  await join(cs, channels.stage);
  const hasH264 = await cs.evaluate(() => RTCRtpSender.getCapabilities("video").codecs.some(c =>
    c.mimeType.toLowerCase() === "video/h264" && /(^|;)\s*packetization-mode=1(?:;|$)/.test(c.sdpFmtpLine ?? "") &&
    /(^|;)\s*profile-level-id=(42[eEcC]0|640[cC]|6400)[0-9a-fA-F]{2}(?:;|$)/.test(c.sdpFmtpLine ?? "")));

  const encoding = await cs.evaluate(async () => {
    const codecs = RTCRtpSender.getCapabilities("video")?.codecs.filter(c => c.mimeType.toLowerCase() === "video/h264") ?? [];
    const probes = await Promise.all([30, 60].map(async framerate => {
      try {
        const info = await navigator.mediaCapabilities.encodingInfo({ type: "webrtc", video: {
          contentType: "video/H264;profile-level-id=42e02a;packetization-mode=1",
          width: 1920, height: 1080, framerate, bitrate: 8_000_000,
        } });
        return { framerate, supported: info.supported, smooth: info.smooth, powerEfficient: info.powerEfficient };
      } catch (error) { return { framerate, error: String(error) }; }
    }));
    return { codecs, probes };
  });
  const capability = { check: "H.264 browser capability", engine: cs.engine, available: hasH264, ...encoding,
    gap: hasH264 ? null : "Browser exposes no H.264 encoder; actual VP9 fallback is required below." };
  checks.push(capability);
  console.log(JSON.stringify(capability));
  if (process.env.E2E_SCREEN_REQUIRE_H264 === "1") assert.ok(hasH264, "this CI gate requires an H.264 encoder");

  // A real tab may adapt its capture size during bandwidth startup. The fixed
  // canvas run separately requires full 1080p60 despite the viewers' SDP level 3.1.
  const requiredSize = fixedCanvas ? SOURCE_SIZES[1] : null;
  const csShare = await share(cs, chromiumTab ? "Sharing a browser tab" : "Sharing your screen", "Game");
  checks.push({ check: "capture source", mode: chromiumTab ? "real tab" : "canvas", fixedCanvas });
  // Exercise encoder absence independently of the H.264 primary. A WebKit
  // sender may select High while Linux Chromium exposes only Constrained Baseline;
  // the generic lifecycle matrix must not assume those H.264 profiles intersect.
  const ws = await device(engine("webkit"), web, server, "ws-canvas", { frequency: 550, canvasId: 2, sendCodec: "vp9" });
  await join(ws, channels.stage);
  const wsShare = await share(ws, "Sharing your screen");

  // A viewer in another channel's voice watches both.
  await join(cv, channels.hall);
  await watch(cv, "cs-tab");
  await sees(cv, csShare, 1, "chromium → chromium, viewer in another channel");
  await codecFlow(cs, cv, csShare, hasH264 ? "video/h264" : "video/vp9", requiredSize);
  checks.push({ check: "selected browser codec verified through SFU", codec: hasH264 ? "h264" : "vp9", fixedCanvas });
  if (hasH264) {
    const incompatible = await device(engine("chromium"), web, server, "vp9-only-viewer", { receiveCodec: "vp9" });
    await watch(incompatible, "cs-tab");
    await incompatible.getByText("Your browser cannot decode this screen share. Ask the sharer to use a compatible browser or codec.").waitFor();
    assert.equal(await shareOf(incompatible, "cs-tab"), csShare, "a codec refusal does not remove the running share");
    assert.equal(await session(incompatible, (_, screen) => screen.watches.length), 0);
    checks.push({ check: "incompatible viewer is explicitly refused", ok: true });
  }
  await watch(cv, "ws-canvas");
  await sees(cv, wsShare, 2, "webkit → chromium");
  await codecFlow(ws, cv, wsShare, "video/vp9");
  await bar(cs).getByText("1 watching").waitFor({ timeout: 10_000 });

  // A late joiner, in no voice at all, learns of both from the Welcome.
  const wv = await device(engine("webkit"), web, server, "wv-late", { frequency: 660 });
  assert.equal(await session(wv, s => s.clients[s.me.session].channel), null, "the late viewer is not in voice");
  assert.equal(await shareOf(wv, "cs-tab"), csShare);
  await watch(wv, "cs-tab");
  await sees(wv, csShare, 1, "chromium → webkit, late joiner outside voice");
  await codecFlow(cs, wv, csShare, hasH264 ? "video/h264" : "video/vp9", requiredSize);
  await watch(wv, "ws-canvas");
  await sees(wv, wsShare, 2, "webkit → webkit");
  await codecFlow(ws, wv, wsShare, "video/vp9");
  await bar(ws).getByText("2 watching").waitFor({ timeout: 10_000 });

  // Both media players remount when the real Stage swaps focus. WebKit needs a
  // post-attachment keyframe even though both original receivers keep decoding.
  const beforeFocus = await session(wv, (_, screen) => ({ transport: screen.transport, watches: screen.watches }));
  await wv.evaluate(share => document.querySelector(`video[data-share="${share}"]`).closest('[role="button"]').click(), csShare);
  await sees(wv, csShare, 1, "stage switch renders the focused share in a replacement player");
  await sees(wv, wsShare, 2, "stage switch renders the thumbnail in a replacement player");
  assert.deepEqual(await session(wv, (_, screen) => ({ transport: screen.transport, watches: screen.watches })), beforeFocus, "a focus change keeps the transport and watches");

  // Start → stop → start while watching two other shares: the watch transport is untouched.
  const c3 = await device(engine("chromium"), web, server, "c3-restart", { frequency: 700, canvasId: [3, 6], sendCodec: "vp9" });
  await join(c3, channels.stage);
  await watch(c3, "cs-tab");
  await sees(c3, csShare, 1, "restarting sharer watches the tab share");
  await watch(c3, "ws-canvas");
  await sees(c3, wsShare, 2, "restarting sharer watches the canvas share");
  const transport = await session(c3, (_, screen) => screen.transport);
  const first = await share(c3, "Sharing your screen");
  await watch(cv, "c3-restart");
  await sees(cv, first, 3, "first share of the restarting sharer");
  await codecFlow(c3, cv, first, "video/vp9");
  // The WebKit sharer has no other watches: its receive transport may renew safely.
  await watch(ws, "c3-restart");
  await sees(ws, first, 3, "sole watch before slot reuse");
  await stopSharing(c3);
  await waitFor(async () => (await shareOf(cv, "c3-restart")) === null, "the first share ends for viewers");
  await waitFor(() => session(ws, (_, screen) => screen.watches.length === 0), "the sole watch ends");
  const second = await share(c3, "Sharing your screen");
  assert.notEqual(second, first, "a new share gets a new id");
  const oldViewerRun = await session(cv, (_, screen) => screen.transport.run);
  await cv.evaluate(share => {
    const video = document.querySelector(`video[data-share="${share}"]`);
    const observation = window.__otherWatch = { video, stream: video.srcObject, visible: true, counters: [] };
    const sample = () => {
      const current = document.querySelector(`video[data-share="${share}"]`);
      observation.visible &&= current === video && current.srcObject === observation.stream && getComputedStyle(current).opacity === "1";
      const frame = window.__readBarcode(current);
      if (frame && !frame.invalid) observation.counters.push(frame.counter);
      observation.sampler = requestAnimationFrame(sample);
    };
    sample();
  }, wsShare);
  const oldSoloRun = await session(ws, (_, screen) => screen.transport.run);
  await ws.evaluate(share => {
    window.__reuseFrames = [];
    const sample = () => {
      const video = document.querySelector(`video[data-share="${share}"]`);
      if (video && getComputedStyle(video).opacity === "1") {
        const frame = window.__readBarcode(video);
        if (frame && !frame.invalid) window.__reuseFrames.push(frame.id);
      }
      window.__reuseSampler = requestAnimationFrame(sample);
    };
    sample();
  }, second);
  await watch(cv, "c3-restart");
  await watch(ws, "c3-restart");
  // The second share has its own source (6, another shape): its frames cannot be the first's.
  await sees(cv, second, 6, "second share of the restarting sharer");
  assert.equal(await session(cv, (_, screen) => screen.transport.run), oldViewerRun, "another active watch prevents renewal");
  assert.ok(await cv.evaluate(share => {
    const video = document.querySelector(`video[data-share="${share}"]`);
    return video === window.__otherWatch.video && video.srcObject === window.__otherWatch.stream && getComputedStyle(video).opacity === "1";
  }, wsShare), "the other active watch keeps its video element, stream and visibility");
  checks.push({ check: "slot reuse leaves other active watches uninterrupted", ok: true });
  await sees(ws, second, 6, "sole watch after slot reuse");
  const otherWatch = await cv.evaluate(() => {
    cancelAnimationFrame(window.__otherWatch.sampler);
    return { visible: window.__otherWatch.visible, frames: new Set(window.__otherWatch.counters).size };
  });
  assert.ok(otherWatch.visible, "the untouched watch stays visible throughout sole-watch renewal");
  if (paintsUnreliably(cv)) checks.push({ check: "untouched watch's barcode progresses during sole-watch renewal", skipped: true, reason: "WebKit on Linux paints remote video unreliably" });
  else {
    assert.ok(otherWatch.frames > 1, "the untouched watch's frames progress during sole-watch renewal");
    checks.push({ check: "untouched watch stays visible and frames progress during sole-watch renewal", ok: true });
  }
  const reuseFrames = await ws.evaluate(() => {
    cancelAnimationFrame(window.__reuseSampler);
    return window.__reuseFrames;
  });
  if (paintsUnreliably(ws)) checks.push({ check: "sole reused slot displays only the new share's barcode", skipped: true, reason: "WebKit on Linux paints remote video unreliably" });
  else {
    assert.ok(reuseFrames.length > 0 && reuseFrames.every(id => id === 6), `sole reused slot showed another share: ${reuseFrames}`);
    checks.push({ check: "sole reused slot displays only the new share's barcode", ok: true });
  }
  assert.ok(await session(ws, (_, screen) => screen.transport.run) > oldSoloRun, "a sole reused slot renews its receiver");
  checks.push({ check: "sole slot reuse renews its receiver", ok: true });
  await session(ws, (_, __, controller, share) => controller.screen.unwatch(share), second);
  await sees(c3, csShare, 1, "still watching the tab share after start → stop → start");
  await sees(c3, wsShare, 2, "still watching the canvas share after start → stop → start");
  assert.deepEqual(await session(c3, (_, screen) => screen.transport), transport, "the watch transport stayed the same");

  // The slot limit: four watched shares fill the watch transport.
  const c4 = await device(engine("chromium"), web, server, "c4-canvas", { frequency: 750, canvasId: 4 });
  await join(c4, channels.stage);
  const c4Share = await share(c4, "Sharing your screen");
  const c5 = await device(engine("chromium"), web, server, "c5-hall", { frequency: 800, canvasId: 5 });
  await join(c5, channels.hall);
  const c5Share = await share(c5, "Sharing your screen");
  await watch(cv, "c4-canvas");
  await sees(cv, c4Share, 4, "fourth slot");
  await watch(cv, "c5-hall");
  await cv.getByText("You can watch up to 4 screen shares at once. Stop watching one first.").waitFor({ timeout: 10_000 });
  assert.equal((await session(cv, (_, screen) => screen.watches.length)), 4, "still four watches");
  await cv.getByRole("button", { name: "Stop watching" }).click();
  await waitFor(async () => (await session(cv, (_, screen) => screen.watches.length)) === 3, "a slot is freed");
  await watch(cv, "c5-hall");
  await sees(cv, c5Share, 5, "a freed slot takes another share");
  checks.push({ check: "slot limit", ok: true });

  // A moderator stops someone's share from the stage.
  await cv.getByRole("button", { name: "ws-canvas's screen" }).click();
  await cv.getByRole("heading", { name: "ws-canvas's screen" }).waitFor();
  await cv.getByRole("button", { name: "Stop screen share" }).click();
  await ws.getByText("cv-mod stopped your screen share.").waitFor({ timeout: 10_000 });
  await bar(ws).waitFor({ state: "detached" });
  await waitFor(async () => (await wv.locator(`video[data-share="${wsShare}"]`).count()) === 0, "the stopped share leaves the late viewer's stage");
  assert.equal(await shareOf(wv, "ws-canvas"), null);
  checks.push({ check: "moderator stop", ok: true });

  // Encrypted channels refuse until encrypted shares land: the button says so, nothing is sent.
  await join(c4, channels.secret);
  await waitFor(async () => (await shareOf(cv, "c4-canvas")) === null, "leaving the channel ends the share");
  const soon = c4.getByRole("button", { name: "Screen sharing in encrypted channels is coming soon." });
  await soon.waitFor({ timeout: 20_000 });
  assert.equal(await soon.getAttribute("aria-disabled"), "true");
  await soon.click({ force: true });
  await sleep(500);
  assert.equal(await session(c4, (_, screen) => screen.own), null, "no capture in an encrypted channel");
  assert.equal(await c4.getByRole("dialog", { name: "Share your screen" }).count(), 0);
  checks.push({ check: "encrypted channel refuses", ok: true });

  // Exercise isolation first, then corroborate the same failure across every active
  // viewer. Only the watchdog counters stall; real receiver stats/barcodes still
  // prove each replacement delivers video and keeps the original watch slots.
  if (hasH264) {
    const stalled = async (page, enabled) => {
      const slot = await session(page, (_, screen, __, share) => screen.watches.find(w => w.share === share)?.slot, csShare);
      await page.evaluate(({ slot, enabled }) => {
        window.__failH264Decode = enabled;
        for (const pc of window.__pcs) {
          const mid = pc.getTransceivers().filter(t => t.receiver.track.kind === "video")[slot]?.mid;
          if (mid == null || pc.__recoveryStats) continue;
          pc.__recoveryStats = pc.getStats.bind(pc);
          pc.getStats = async (...args) => {
            const report = await pc.__recoveryStats(...args), altered = new Map();
            report.forEach((s, key) => altered.set(key, window.__failH264Decode && s.mid === mid && s.type === "inbound-rtp" && /video\/h264/i.test(report.get(s.codecId)?.mimeType ?? "") ? { ...s, framesDecoded: 0 } : s));
            if (window.__traceScreen) window.__traceScreen("watchdog-stats", {
              mid, enabled: window.__failH264Decode,
              streams: [...report.values()].filter(s => s.mid === mid && s.type === "inbound-rtp").map(s => ({
                codec: report.get(s.codecId)?.mimeType, packets: s.packetsReceived,
                actualFrames: s.framesDecoded, reportedFrames: altered.get(s.id)?.framesDecoded,
              })),
            });
            return altered;
          };
        }
      }, { slot, enabled });
    };
    await cs.evaluate(() => { window.__publisherBeforeIsolation = window.__pcs.find(pc => pc.connectionState === "connected" && pc.getSenders().some(s => s.track?.kind === "video")); });
    await stalled(wv, true);
    await waitFor(async () => !(await session(wv, (_, screen, __, share) => screen.watches.some(w => w.share === share), csShare)), "one decode reporter is isolated");
    await wv.getByText("Your browser cannot decode this screen share. Ask the sharer to use a compatible browser or codec.", { exact: true }).waitFor();
    await codecFlow(cs, c3, csShare, "video/h264");
    assert.equal(await cs.evaluate(() => window.__publisherBeforeIsolation?.connectionState), "connected", "one reporter does not replace the publisher");
    await sees(c3, csShare, 1, "healthy viewer survives another viewer's decode failure");
    assert.equal(await ownShare(cs), csShare, "one reporter never replaces the share");
    checks.push({ check: "one reporter is isolated while healthy viewers keep the existing H.264 publisher", ok: true });
    await stalled(wv, false);
    await watch(wv, "cs-tab");
    await sees(wv, csShare, 1, "isolated viewer may explicitly retry");
    // Let the reporter's ten-second server throttle expire before an independent
    // all-viewer incident; otherwise it cannot corroborate the healthy viewers.
    await sleep(10_000);
    // cv released this share during the four-slot limit check above. These
    // are the two remaining active watches; both must corroborate failure.
    const viewers = [wv, c3];
    const before = await Promise.all(viewers.map(page => session(page, (_, screen, __, share) => screen.watches.find(w => w.share === share)?.slot, csShare)));
    await Promise.all(viewers.map(page => stalled(page, true)));
    let capped;
    await waitFor(async () => {
      capped = await cs.evaluate(async () => {
        for (const pc of window.__pcs) {
          if (pc.connectionState !== "connected") continue;
          const sender = pc.getSenders().find(s => s.track?.kind === "video" && s.getParameters().encodings[0]?.maxFramerate === 30);
          if (!sender) continue;
          const report = await pc.getStats();
          for (const s of report.values()) if (s.type === "outbound-rtp" && s.kind === "video" && s.framesEncoded > 0 && report.get(s.codecId)?.mimeType?.toLowerCase() === "video/h264")
            return { width: s.frameWidth, height: s.frameHeight, maxFramerate: sender.getParameters().encodings[0].maxFramerate };
        }
        return null;
      });
      return capped?.width > 0 && capped.width <= 1280 && capped.height <= 720;
    }, "decode recovery applies the lower H.264 resolution and rate cap");
    checks.push({ check: "H.264 decode recovery cap", ...capped });
    await codecFlow(cs, wv, csShare, "video/h264");
    await codecFlow(cs, wv, csShare, "video/vp9");
    assert.equal(await ownShare(cs), csShare, "recovery retains the original share id");
    for (const [index, page] of viewers.entries()) {
      assert.equal(await session(page, (_, screen, __, share) => screen.watches.find(w => w.share === share)?.slot, csShare), before[index], "corroborated recovery retains every viewer's slot");
      await sees(page, csShare, 1, "VP9 decode after corroborated watchdog recovery");
    }
    checks.push({ check: "corroborated watchdog caps H.264 then selects VP9 without stopping any watch", ok: true });
  }

  // A page stopping capture emits no `ended` event. The existing publisher poll
  // must end the share and free every viewer's slot through the normal path.
  await cs.evaluate(() => {
    const sender = window.__pcs.filter(pc => pc.connectionState !== "closed")
      .flatMap(pc => pc.getSenders()).find(s => s.track?.kind === "video" && s.track.readyState === "live");
    if (!sender?.track) throw new Error("no live screen capture to stop");
    sender.track.stop();
  });
  await waitFor(async () => (await ownShare(cs)) === null, "silent track stop ends the share", 10_000);
  for (const viewer of [cv, wv, c3]) {
    await waitFor(async () => !(await session(viewer, (_, screen, __, share) => screen.watches.some(w => w.share === share) || screen.slots.includes(share), csShare)), "silent track stop releases viewer slots", 10_000);
  }
  checks.push({ check: "page-initiated track.stop ends the share and releases watch slots", ok: true });

  const problems = opened.flatMap(p => errors(p).map(e => `${p.deviceName}: ${e}`));
  return problems;
}

const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-screen-e2e-"));
const browsers = {};
let failure = null, warnings = [];
try {
  const webPort = await port();
  const web = `http://127.0.0.1:${webPort}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", path.join(dataDir, "dist")], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, `e2e web build failed:\n${build.stdout}${build.stderr}`);
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start("vite", process.execPath, [viteCli, "preview", "--outDir", path.join(dataDir, "dist"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web preview", 30_000);
  const binary = process.env.GWAR_SERVER_BIN ?? path.join(repoRoot, "target/debug/gwar-server");
  const serverPort = await port();
  const ws = `ws://127.0.0.1:${serverPort}/ws`;
  const server = start("gwar-server", binary, ["--data-dir", path.join(dataDir, "server"), "--http", `127.0.0.1:${serverPort}`, "--media", `127.0.0.1:${serverPort}`, "--public-ip", "127.0.0.1", "--public-endpoint", ws, "--web-origin", web, "--no-connect"]);
  await waitFor(() => /redeem this one-time admin token in your client: (\S+)/.test(server.output), "server admin token", 60_000);
  const token = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)[1];
  if (engines.includes("chromium")) {
    browsers.chromium = await chromium.launch({ channel: "chromium", headless: true, args: ["--autoplay-policy=no-user-gesture-required", `--auto-select-tab-capture-source-by-title=${SOURCE_TITLE}`] });
  }
  if (engines.includes("webkit")) browsers.webkit = await webkit.launch({ headless: true });
  warnings = await run(browsers, web, ws, token);
} catch (error) {
  failure = error;
  for (const child of children) {
    const codecs = child.output.split("\n").filter(line => line.includes("screen codec incompatible"));
    if (codecs.length) console.error(JSON.stringify({ process: child.taskName, codecs }));
  }
  for (const page of opened) {
    const state = await session(page, (s, screen) => ({ own: screen.own && { ...screen.own, stream: !!screen.own.stream }, transport: screen.transport, watches: screen.watches, slots: screen.slots, shares: Object.keys(s.shares) })).catch(e => String(e));
    const rtc = await diagnostics(page).catch(e => String(e));
    console.error(JSON.stringify({ device: page.deviceName, state, rtc, errors: page.errors.slice(-20) }));
  }
} finally {
  try {
    if (traceDir) {
      mkdirSync(traceDir, { recursive: true });
      for (const child of children) writeFileSync(path.join(traceDir, `${child.taskName}.log`), child.output);
      for (const page of opened) {
        const state = await session(page, (s, screen) => ({ own: screen.own && { ...screen.own, stream: !!screen.own.stream }, transport: screen.transport, watches: screen.watches, slots: screen.slots, shares: Object.keys(s.shares) })).catch(e => String(e));
        const rtc = await diagnostics(page).catch(e => String(e));
        const trace = await page.evaluate(() => window.__screenTrace).catch(e => String(e));
        writeFileSync(path.join(traceDir, `${page.deviceName}.json`), JSON.stringify({ state, rtc, trace }, null, 2));
      }
    }
  } finally {
    for (const browser of Object.values(browsers)) await browser.close().catch(() => {});
    await stopProcesses(children);
    rmSync(dataDir, { recursive: true, force: true });
  }
}
for (const check of checks) console.log(JSON.stringify(check));
if (warnings.length) console.log(JSON.stringify({ warnings }));
if (failure) { console.error(failure.stack ?? failure); process.exit(1); }
console.log(JSON.stringify({ status: "pass", engines }));
