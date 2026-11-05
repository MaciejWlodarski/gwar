#!/usr/bin/env node
/**
 * Lip-sync of a watched share, measured (docs/share-audio.md "Lip-sync"). A real tab shows a barcode
 * that changes at the same instant as a tone burst, once a second. Each viewer records when it
 * presents the new picture and when the burst comes out, so every burst gives one offset:
 * positive means the sound lags the picture. With Chromium viewers nothing reaches the Mac's
 * speakers: on macOS it runs with `--disable-audio-output`, and the desktop viewer's speaker is
 * `native_bridge`'s virtual device. WebKit viewers opened the speakers even as desktop viewers, so
 * those run on CI only. Linux WebKit has no `RTCRtpScriptTransform`, so its desktop viewer cannot keep
 * the sound from the webview's decoder and, as the app does there, plays the picture only: the run
 * checks that it shows the picture and offers no sound, and measures nothing for it.
 *
 * Build once: cargo build --release -p gwar-client --no-default-features --example web_latency --example native_bridge
 *   SYNC_NET=normal|jitter|<LATENCY_NET spec>   the link in front of every viewer (default: normal)
 *   SYNC_VIEWERS=browser,desktop                who watches, side by side
 *   SYNC_SECONDS=30                             how long they watch
 *   SYNC_NOHOLD=1 / SYNC_NOFILTER=1             sync on, but without the start's sound hold / the late-frame filter (A/B)
 *   SYNC_NOWAIT=1                               sync on, but nothing ever waits for the other side (A/B)
 *   SYNC_SERVER_LOG=path                        the fixture's log there, with RUST_LOG (e.g. gwar_server::media::screen=debug)
 *   SYNC_ENGINE=chromium|webkit                 the viewers' engine (default: chromium)
 *   SYNC_OFF=1                                  main's playback, for a before: no picture wait, no sound
 *                                               hold, no late-frame filter (so no stall report), no held
 *                                               start; the run fails if any of these was not seen refused
 *   SYNC_ADD=1                                  the share starts with no sound and gets it 8 s into the watch
 *   SYNC_RETURN=1                               the sound stops 8 s into the watch and returns 5 s later
 *   SYNC_OUT=path.json                          every burst and the summaries
 *   SYNC_FAKE_TAB=1                             the sharer's page is the source (Linux, where a tab's sound is not captured)
 *   SYNC_WORKER_FAIL=load|crash                 the late-frame worker never loads, or dies 6 s into the watch after
 *                                               pictures flowed; the run fails unless every viewer's picture (and a
 *                                               browser viewer's sound) still plays after it
 */
import { spawn, spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { createWriteStream, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { chromium, webkit } from "playwright";
import { chooseFirstRunName } from "./first-run.mjs";
import { stopProcesses } from "./processes.mjs";
import { ONSET_QUIET_MS, SOUND_CODES, baselineFaults, offsetSummary, pairBursts } from "./share-sync-judge.mjs";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(webRoot, "../..");
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, "target");
const fixtureBinary = process.env.SYNC_FIXTURE ?? path.join(targetDir, "release/examples/web_latency");
const bridgeBinary = process.env.NATIVE_BRIDGE_BIN ?? path.join(targetDir, "release/examples/native_bridge");
const seconds = Number(process.env.SYNC_SECONDS ?? 30);
const NETS = { normal: "seed=1", jitter: "seed=42;down:delay=20ms,jitter=10ms,keep_order" };
const VIEWERS = (process.env.SYNC_VIEWERS ?? "browser,desktop").split(",").map(s => s.trim()).filter(Boolean);
const netName = process.env.SYNC_NET ?? "normal";
const netSpec = NETS[netName] ?? netName;
const SOURCE_TITLE = "Gwar sync source";
const children = [];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function port() {
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  return port;
}
function start(command, args, options = {}) {
  const child = spawn(command, args, { cwd: repoRoot, stdio: ["pipe", "pipe", "pipe"], detached: true, ...options });
  child.output = "";
  child.stderr.on("data", chunk => { child.output = (child.output + chunk).slice(-4000); });
  children.push(child);
  return child;
}
async function waitFor(check, what, timeout = 30_000) {
  const started = Date.now();
  for (;;) {
    if (await check()) return;
    if (Date.now() - started > timeout) throw new Error(`timed out: ${what}`);
    await sleep(150);
  }
}

/**
 * The source tab. A burst is planned 100 ms ahead so the audio graph can start it on its exact
 * sample, and the first draw at or after that instant carries the burst's number. Its sound is four
 * pulses whose gaps carry the number too, so sound late by any whole seconds stays with its burst.
 * A 5 ms timer keeps the draw within a few milliseconds whatever the compositor's rhythm.
 */
const sourceScript = `
function startSource(canvas, sink) {
const draw2d = canvas.getContext("2d");
const audio = new AudioContext({ sampleRate: 48000 }), tone = audio.createOscillator(), gain = audio.createGain();
tone.frequency.value = 997; gain.gain.value = 0; tone.connect(gain).connect(sink(audio)); tone.start();
window.__bursts = []; window.__toneContext = audio;
let burst = 0, plan = null, nextPlan = 0, tick = 0;
const CODES = ${JSON.stringify(SOUND_CODES)};
function pulse(when) { gain.gain.setValueAtTime(0.2, when); gain.gain.setValueAtTime(0, when + 0.04); }
function bits(n) { const out = []; for (let i = 11; i >= 0; i--) out.push((n >> i) & 1); const check = (n ^ (n >> 4) ^ (n >> 8) ^ 10) & 15; for (let i = 3; i >= 0; i--) out.push((check >> i) & 1); return out; }
function paint() {
  const { width: w, height: h } = canvas;
  tick++;
  draw2d.fillStyle = "#777"; draw2d.fillRect(0, 0, w, h);
  draw2d.fillStyle = "hsl(" + (tick * 7) % 360 + " 70% 50%)"; draw2d.fillRect((tick * 9) % w, h / 2, w / 8, h / 4);
  // Busy content, so the picture costs what a real share costs and not 200 kb/s.
  for (let i = 0; i < 300; i++) { draw2d.fillStyle = "hsl(" + Math.random() * 360 + " 60% " + (30 + Math.random() * 40) + "%)"; draw2d.fillRect(Math.random() * w, h / 4 + Math.random() * h * 3 / 4, 24, 24); }
  bits(burst).forEach((bit, i) => { draw2d.fillStyle = bit ? "#000" : "#fff"; draw2d.fillRect(i * w / 16, 0, w / 16 + 1, h / 4); });
}
paint();
setInterval(() => {
  const now = performance.now();
  if (!plan && audio.state === "running" && now >= nextPlan) {
    const at = now + 100, stamp = audio.getOutputTimestamp();
    const when = stamp.contextTime + (at - stamp.performanceTime) / 1000;
    let next = when;
    pulse(next);
    for (const gap of CODES[((burst + 1) & 4095) % CODES.length]) pulse(next += gap / 1000);
    plan = { at };
    nextPlan = at + 900;
  }
  if (plan && now >= plan.at) {
    burst = (burst + 1) & 4095;
    window.__bursts.push({ burst, plannedAt: performance.timeOrigin + plan.at, drawnAt: performance.timeOrigin + now });
    plan = null;
  }
  paint();
}, 5);
}`;
const sourcePage = `<!doctype html><title>${SOURCE_TITLE}</title><style>html,body{margin:0;height:100%;overflow:hidden;background:#777}canvas{display:block;width:100vw;height:100vh}</style>
<canvas width="1280" height="720"></canvas><script>${sourceScript}
startSource(document.querySelector("canvas"), audio => audio.destination);
</script>`;

/**
 * For Linux, whose Chromium gives a tab's picture but not its sound: the sharer's own page runs the
 * source and \`getDisplayMedia\` answers with its canvas and its tone, both reported as a tab. The
 * tone goes to the stream only, never to a speaker. What it leaves out is the browser's tab capture;
 * everything from the sharer's encoder to the viewers' playback is the same.
 */
const fakeTab = `${sourceScript}
(() => {
  let made = null;
  MediaDevices.prototype.getDisplayMedia = async options => {
    if (!made) {
      const canvas = document.createElement("canvas");
      [canvas.width, canvas.height] = [1280, 720];
      canvas.style.cssText = "position:fixed;left:0;bottom:0;width:64px;height:36px;opacity:0.02;pointer-events:none;z-index:-1";
      document.body.appendChild(canvas);
      let out = null;
      startSource(canvas, audio => (out = audio.createMediaStreamDestination()));
      await window.__toneContext.resume();
      made = { video: canvas.captureStream(60).getVideoTracks()[0], audio: out.stream.getAudioTracks()[0] };
    }
    // A tab's capture reports what it applied, as a real one does: the surface, and for its sound
    // \`restrictOwnAudio\` when it was asked (the app drops tab sound that does not keep its own out).
    const tab = (track, applied) => {
      const settings = track.getSettings.bind(track);
      track.getSettings = () => ({ ...settings(), displaySurface: "browser", ...applied });
      return track;
    };
    const restrictOwnAudio = typeof options?.audio === "object" && options.audio.restrictOwnAudio === true;
    return new MediaStream([tab(made.video.clone()), ...(options?.audio ? [tab(made.audio.clone(), restrictOwnAudio ? { restrictOwnAudio } : {})] : [])]);
  };
})();`;

/** Installed in every viewer: the presentation of each new burst number. */
function viewerPatch() {
  window.__presented = [];
  window.__onsets = [];
  window.__pcs = [];
  /** When a watch's sound can first be heard, against its first picture. */
  window.__audible = null;
  window.__granted = null;
  /** What a `SYNC_OFF` run must have kept out; judged by `baselineFaults`. */
  window.__baseline = { filtersRun: 0, filtersRefused: 0, soundHolds: 0 };
  if (window.RTCRtpScriptTransform) window.RTCRtpScriptTransform = new Proxy(window.RTCRtpScriptTransform, {
    construct(target, args) {
      // Only the late-frame filter names a kind; the desktop's sound path stays.
      if (args[1]?.kind) {
        // The app plays on as it would have: the same catch as an engine without transforms.
        if (window.__syncOff || window.__noFilter) { window.__baseline.filtersRefused++; throw new Error("the baseline has no filter"); }
        window.__baseline.filtersRun++;
      }
      return new target(...args);
    },
  });
  const audible = setInterval(async () => {
    const watch = (await window.__gwarE2eImport("/src/state/screen.ts")).useScreen.getState().watches[0];
    if (watch?.status === "watching") window.__granted ??= Date.now();
    if (watch?.status === "watching" && !watch.soundHeld) { window.__audible = Date.now(); clearInterval(audible); }
  }, 10);
  /** When the late-frame filter said the picture's link is back (it then asks for a keyframe). */
  window.__linkBacks = [];
  /** Every word of the filter, picture and sound: what it dropped since the last, and when. */
  window.__sheds = [];
  /** The late-frame workers, for `SYNC_WORKER_FAIL`. */
  window.__lateFrameWorkers = [];
  window.Worker = new Proxy(window.Worker, {
    construct(target, args) {
      const filter = args[1]?.name === "gwar-late-frames";
      // A chunk that is not there, as after a deploy while the tab stayed open.
      const worker = new target(...(filter && window.__workerFail === "load" ? ["/late-frames-worker-gone.js", args[1]] : args));
      if (filter) window.__lateFrameWorkers.push(worker);
      if (filter) worker.addEventListener("message", event => {
        if (event.data?.slot === undefined) return;
        window.__sheds.push(event.data);
        if (event.data.kind === "video") window.__linkBacks.push(Date.now());
      });
      return worker;
    },
  });
  /** Each look's lag as the controller measured it (before its aim), and every wait it asked for. */
  window.__looks = [];
  window.__waits = [];
  window.RTCPeerConnection = new Proxy(window.RTCPeerConnection, {
    construct(target, args) { const pc = new target(...args); window.__pcs.push(pc); return pc; },
  });
  /**
   * What each connection's receivers played so far, by its place in `__pcs`: pictures decoded, and
   * sound that was not concealment. A closed connection counts what it had.
   */
  window.__played = async () => {
    const run = (await window.__gwarE2eImport("/src/state/screen.ts")).useScreen.getState().transport.run;
    const pcs = {};
    for (const [i, pc] of window.__pcs.entries()) {
      if (pc.connectionState !== "connected") continue;
      const played = pcs[i] = { video: 0, audio: 0 };
      for (const stat of (await pc.getStats()).values()) {
        if (stat.type !== "inbound-rtp") continue;
        if (stat.kind === "video") played.video += stat.framesDecoded ?? 0;
        else played.audio += (stat.totalSamplesReceived ?? 0) - (stat.concealedSamples ?? 0);
      }
    }
    return { run, pcs };
  };
  /** Every second, what the receivers say: the picture's and the sound's delay, and repairs. */
  window.__health = [];
  window.__followHealth = () => setInterval(async () => {
    const row = { at: Date.now(), watch: (await window.__gwarE2eImport("/src/state/screen.ts")).useScreen.getState().watches.map(w => w.status).join() };
    for (const pc of window.__pcs.filter(pc => pc.connectionState === "connected")) {
      row.waits = [row.waits, ...pc.getReceivers().filter(r => r.jitterBufferTarget > 0).map(r => `${r.track.kind} ${r.jitterBufferTarget}`)].filter(Boolean).join();
      for (const stat of (await pc.getStats()).values()) {
        if (stat.type !== "inbound-rtp" || !stat.bytesReceived) continue;
        row[stat.kind] = { bytes: stat.bytesReceived, frames: stat.framesDecoded, dropped: stat.framesDropped, keys: stat.keyFramesDecoded, nacks: stat.nackCount, plis: stat.pliCount, freezes: stat.freezeCount, delay: stat.jitterBufferDelay, emitted: stat.jitterBufferEmittedCount, target: stat.jitterBufferTargetDelay, minimum: stat.jitterBufferMinimumDelay, lost: stat.packetsLost, concealed: stat.concealedSamples, packets: stat.packetsReceived, received: stat.framesReceived, processing: stat.totalProcessingDelay, decode: stat.totalDecodeTime, assembly: stat.totalAssemblyTime, assembled: stat.framesAssembledFromMultiplePackets, samples: stat.totalSamplesReceived };
      }
    }
    window.__health.push(row);
  }, 1000);
  const small = document.createElement("canvas");
  small.width = 320; small.height = 180;
  const ctx = small.getContext("2d", { willReadFrequently: true });
  const read = video => {
    ctx.drawImage(video, 0, 0, 320, 180);
    const data = ctx.getImageData(0, 22, 320, 1).data;
    const bits = [];
    for (let i = 0; i < 16; i++) {
      const x = Math.floor((i + 0.5) * 20) * 4;
      bits.push((data[x] + data[x + 1] + data[x + 2]) / 3 < 128 ? 1 : 0);
    }
    const num = (from, n) => bits.slice(from, from + n).reduce((v, b) => v * 2 + b, 0);
    const value = num(0, 12);
    return ((value ^ (value >> 4) ^ (value >> 8) ^ 10) & 15) === num(12, 4) ? value : null;
  };
  /** Follows the first video on the page, whichever element the app gives it. */
  window.__followPicture = () => {
    let last = null, video = null;
    const frame = (_, meta) => {
      const burst = read(video);
      if (burst !== null && burst !== last) {
        last = burst;
        window.__presented.push({ burst, at: performance.timeOrigin + meta.expectedDisplayTime, rtp: meta.rtpTimestamp, capture: meta.captureTime, receive: meta.receiveTime });
      }
      video.requestVideoFrameCallback(frame);
    };
    // The stage can replace its element during a watch.
    setInterval(() => {
      const current = document.querySelector("video[data-share]");
      if (current === video) return;
      video = current;
      video?.requestVideoFrameCallback(frame);
    }, 100);
  };
  /** A browser viewer's sound: the same track its `<audio>` plays, through a silent tap. */
  window.__followSound = (element, quietSamples) => {
    void window.__syncAudio?.close();
    const context = new AudioContext({ sampleRate: 48000 });
    void context.resume();
    const source = context.createMediaStreamSource(element.srcObject);
    const tap = context.createScriptProcessor(256, 2, 1);
    const mute = context.createGain();
    mute.gain.value = 0;
    let quiet = 0, armed = false;
    tap.onaudioprocess = event => {
      const pcm = event.inputBuffer.getChannelData(0);
      for (let i = 0; i < pcm.length; i++) {
        if (Math.abs(pcm[i]) > 0.02) {
          if (armed) {
            armed = false;
            const stamp = context.getOutputTimestamp();
            const playedAt = event.playbackTime + i / context.sampleRate - pcm.length / context.sampleRate;
            window.__onsets.push(performance.timeOrigin + stamp.performanceTime + (playedAt - stamp.contextTime) * 1000);
          }
          quiet = 0;
        } else if (++quiet > quietSamples) armed = true;
      }
    };
    source.connect(tap); tap.connect(mute).connect(context.destination);
    window.__syncAudio = context;
  };
}

/** One native engine: the desktop viewer's mixer and its virtual speaker, nothing audible. */
function nativeFixture() {
  const child = start(bridgeBinary, ["300"]);
  const pending = new Map();
  let next = 0;
  const fixture = { page: null };
  createInterface({ input: child.stdout }).on("line", line => {
    const message = JSON.parse(line);
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
    child.stdin.write(`${JSON.stringify({ id, command, args })}\n`);
  });
  return fixture;
}

/** `NativeVoiceEngine`'s invoke/listen pair, as in native-voice.mjs. */
function bridgePatch() {
  const listeners = new Map();
  window.__nativeEmit = (event, payload) => { for (const handler of listeners.get(event) ?? []) handler(payload); };
  window.__nativeBridge = async () => ({
    invoke: (command, args) => window.__nativeInvoke(command, args instanceof Uint8Array ? { body: Array.from(args) } : args ?? {}),
    listen: async (event, handler) => {
      const set = listeners.get(event) ?? new Set();
      listeners.set(event, set);
      set.add(handler);
      return () => set.delete(handler);
    },
  });
}

async function device(browser, web, server, name, native = false) {
  const context = await browser.newContext({ locale: "en-US", viewport: { width: 1280, height: 800 } });
  await context.route("**/*", route => (new URL(route.request().url()).hostname === "127.0.0.1" ? route.continue() : route.abort()));
  if (process.env.SYNC_OFF) await context.addInitScript(() => {
    window.__syncOff = true;
    // Main writes 0 once, at the start of a slot; anything else is a picture wait.
    const target = Object.getOwnPropertyDescriptor(RTCRtpReceiver.prototype, "jitterBufferTarget");
    Object.defineProperty(RTCRtpReceiver.prototype, "jitterBufferTarget", { ...target, set(ms) {
      if (ms === 0) target.set.call(this, 0);
    } });
  });
  if (process.env.SYNC_NOFILTER) await context.addInitScript(() => { window.__noFilter = true; });
  if (process.env.SYNC_NOWAIT) await context.addInitScript(() => { window.__noWait = true; });
  if (process.env.SYNC_WORKER_FAIL) await context.addInitScript(mode => { window.__workerFail = mode; }, process.env.SYNC_WORKER_FAIL);
  await context.addInitScript(viewerPatch);
  if (process.env.SYNC_FAKE_TAB && name === "sync-sharer") await context.addInitScript({ content: fakeTab });
  // The picker's first answer has the tab's picture only, as a share nobody asked sound for.
  if (process.env.SYNC_ADD) await context.addInitScript(() => {
    const pick = navigator.mediaDevices.getDisplayMedia.bind(navigator.mediaDevices);
    let first = true;
    navigator.mediaDevices.getDisplayMedia = options => {
      const audio = !first;
      first = false;
      return pick(audio ? options : { ...options, audio: false });
    };
  });
  const page = await context.newPage();
  if (native) {
    page.native = nativeFixture();
    page.native.page = page;
    await page.exposeFunction("__nativeInvoke", (command, args) => (process.env.SYNC_OFF && command === "share_playback_hold" ? null : page.native.request(command, args)));
    await context.addInitScript(bridgePatch);
  }
  await page.goto(web);
  await page.waitForFunction(() => typeof window.__gwarE2eImport === "function");
  // In the same dispatch as the grant: a later release would leave the desktop slot's gain at 0.
  if (process.env.SYNC_OFF || process.env.SYNC_NOHOLD) await page.evaluate(async () => {
    const { useScreen } = await window.__gwarE2eImport("/src/state/screen.ts");
    useScreen.subscribe(state => {
      for (const watch of state.watches) {
        if (!watch.soundHeld) continue;
        window.__baseline.soundHolds++;
        state.dispatch({ type: "soundReleased", id: watch.id });
      }
    });
  });
  if (native) {
    await page.evaluate(async () => {
      const { NativeVoiceEngine } = await window.__gwarE2eImport("/src/voice/native-engine.ts");
      const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
      controller.useEngine(new NativeVoiceEngine(window.__nativeBridge));
    });
  }
  await chooseFirstRunName(page, name);
  await page.getByLabel("Server address").fill(server);
  await page.getByRole("button", { name: "Settings", exact: true }).first().click();
  await page.getByRole("tab", { name: "Identity", exact: true }).click();
  const nickname = page.getByRole("main", { name: "Settings" }).getByLabel("Nickname", { exact: true });
  await waitFor(async () => (await nickname.inputValue()) !== "", "identity");
  await nickname.fill(name);
  await page.getByRole("main", { name: "Settings" }).getByRole("button", { name: "Save", exact: true }).first().click();
  await page.getByText("Saved", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByText("Not in voice", { exact: true }).waitFor({ timeout: 30_000 });
  page.context_ = context;
  return page;
}

const joinDefault = page => page.evaluate(async () => {
  const { useSession } = await window.__gwarE2eImport("/src/state/stores.ts");
  const { controller } = await window.__gwarE2eImport("/src/state/controller.ts");
  await controller.joinChannel(useSession.getState().server.default_channel);
});

/**
 * Records what the lip-sync controller sees at each look, and with `SYNC_NOWAIT` keeps its waits
 * from reaching either side. Every controller is wrapped at its first look.
 */
const followLooks = page => page.evaluate(async () => {
  const { PictureSync, soundLagMs } = await window.__gwarE2eImport("/src/voice/share-sync.ts");
  const tick = PictureSync.prototype.tick;
  PictureSync.prototype.tick = function (slots) {
    if (!this.__followed) {
      this.__followed = true;
      const link = this.link;
      const last = {};
      const { association, sound, picture, shown, wait } = link;
      link.association = async share => (last.association = await association(share));
      link.sound = async slot => (last.sound = await sound(slot));
      link.picture = slot => {
        const clock = picture(slot);
        if (clock && last.association?.known && last.sound) window.__looks.push({ at: Date.now(), slot, lag: soundLagMs(last.association, last.sound, clock), age: Date.now() - clock.unixMs });
        return clock;
      };
      link.shown = async slot => {
        const held = await shown(slot);
        const look = window.__looks.at(-1);
        if (look?.slot === slot) look.shown = held;
        return held;
      };
      link.wait = (slot, waits) => {
        window.__waits.push({ at: Date.now(), slot, ...waits });
        if (!window.__noWait) wait(slot, waits);
      };
    }
    return tick.call(this, slots);
  };
});

async function viewer(browser, web, server, sharer, kind) {
  const page = await device(browser, web, server, `${kind}-viewer`, kind === "desktop");
  page.kind = kind;
  await followLooks(page);
  // The app's own test for share sound in the desktop app (`shareSoundReceivable`) needs it first.
  page.pictureOnly = kind === "desktop" && !await page.evaluate(() => typeof RTCRtpScriptTransform === "function");
  await joinDefault(page);
  await page.getByRole("button", { name: `${sharer} is sharing their screen. Click to watch.` }).click();
  if (!process.env.SYNC_ADD && !page.pictureOnly) await followSound(page);
  else await page.locator("video").first().waitFor({ state: "attached", timeout: 20_000 });
  await page.evaluate(() => { window.__followPicture(); window.__followHealth(); });
  return page;
}

/** A picture-only viewer must still show the share, and must never offer its sound. */
async function checkPictureOnly(page) {
  assert.ok((await page.evaluate(() => window.__presented.length)) > 0, `the ${page.kind} viewer showed no picture`);
  assert.equal(await page.getByTestId("share-audio-controls").count(), 0, `the ${page.kind} viewer offers sound it cannot play`);
  assert.equal(await page.locator("audio[data-share-audio]").count(), 0, `the ${page.kind} viewer's webview plays the share's sound`);
}

async function followSound(page) {
  if (page.pictureOnly) return;
  if (page.kind === "browser") {
    await page.locator("audio[data-share-audio]").waitFor({ state: "attached", timeout: 20_000 });
    await page.evaluate(quiet => window.__followSound(document.querySelector("audio[data-share-audio]"), quiet), ONSET_QUIET_MS * 48);
  } else await page.getByTestId("share-audio-controls").waitFor({ timeout: 20_000 });
}

/** The sharing bar's Turn on sound, then Details' Choose a tab; the viewers need no new watch. */
async function turnOnSound(sharer, viewers) {
  await sharer.bringToFront();
  await sharer.getByTestId("share-audio-add").first().click();
  await sharer.getByRole("dialog").getByRole("button", { name: "Choose a tab", exact: true }).click();
  await waitFor(() => sharer.evaluate(async () => (await window.__gwarE2eImport("/src/state/screen.ts")).useScreen.getState().own?.audio === true), "sound joined the share", 15_000);
  await sharer.keyboard.press("Escape");
  await Promise.all(viewers.map(followSound));
  return Date.now();
}

/**
 * `SYNC_WORKER_FAIL`: after the late-frame worker failed to load, or died while pictures flowed,
 * every viewer must play on, in place or on a new transport (Opus review r2, S18).
 */
async function workerFailed(pages, mode) {
  await sleep(6_000);
  const failed = await Promise.all(pages.map(page => page.evaluate(async crash => {
    const { run } = await window.__played();
    if (crash) for (const worker of window.__lateFrameWorkers) { worker.terminate(); worker.dispatchEvent(new ErrorEvent("error", { message: "e2e: the worker died" })); }
    return { workers: window.__lateFrameWorkers.length, run };
  }, mode === "crash")));
  const before = await Promise.all(pages.map(page => page.evaluate(() => window.__played())));
  // Long enough for the transport to be taken for stuck and rebuilt, if it is.
  await sleep(9_000);
  const after = await Promise.all(pages.map(page => page.evaluate(() => window.__played())));
  // What each connection's receivers say, for a run that fails.
  const receivers = await Promise.all(pages.map(page => page.evaluate(async () => Promise.all(window.__pcs.map(async pc => ({
    state: pc.connectionState,
    filtered: pc.getReceivers().map(r => `${r.track.kind}:${r.transform ? "on" : "off"}`).join(),
    inbound: pc.connectionState === "closed" ? [] : [...(await pc.getStats()).values()].filter(s => s.type === "inbound-rtp").map(s => ({ kind: s.kind, mid: s.mid, packets: s.packetsReceived, frames: s.framesReceived, decoded: s.framesDecoded, samples: s.totalSamplesReceived, concealed: s.concealedSamples })),
}))))));
  return pages.map((page, i) => {
    const gained = kind => Object.entries(after[i].pcs).reduce((sum, [pc, now]) => sum + now[kind] - (before[i].pcs[pc]?.[kind] ?? 0), 0);
    const result = { viewer: page.kind, mode, workers: failed[i].workers, rebuilt: after[i].run !== failed[i].run, video: gained("video"), audio: gained("audio"), receivers: receivers[i] };
    console.log(JSON.stringify({ workerFail: result }));
    assert.ok(result.video >= 60, `the ${page.kind} viewer's picture stopped after the worker failed (${mode}): ${JSON.stringify(result)}`);
    // Two seconds' worth in nine; a desktop viewer's sound leaves the webview.
    if (page.kind === "browser") assert.ok(result.audio >= 2 * 48_000, `the browser viewer's sound stopped after the worker failed (${mode}): ${JSON.stringify(result)}`);
    return result;
  });
}

async function viewerRecord(page) {
  const record = await page.evaluate(() => ({ presented: window.__presented, onsets: window.__onsets, health: window.__health, audible: window.__audible, granted: window.__granted, baseline: window.__baseline, linkBacks: window.__linkBacks, sheds: window.__sheds, looks: window.__looks, waits: window.__waits }));
  if (page.native) {
    record.onsets = await page.native.request("speaker_onsets");
    record.mixer = await page.native.request("share_stats");
  }
  return record;
}

async function run() {
  const scratch = mkdtempSync(path.join(tmpdir(), "gwar-share-sync-"));
  const webPort = await port();
  const web = `http://127.0.0.1:${webPort}`;
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", path.join(scratch, "dist")], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" }, encoding: "utf8" });
  assert.equal(build.status, 0, build.stderr);
  const viteCli = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  start(process.execPath, [viteCli, "preview", "--outDir", path.join(scratch, "dist"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot, env: { ...process.env, NODE_ENV: "development" } });
  await waitFor(async () => { try { return (await fetch(web)).ok; } catch { return false; } }, "web preview");

  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("LATENCY_") && !key.startsWith("GWAR_")));
  const fixture = start(fixtureBinary, [web, scratch], { env: { ...env, LATENCY_NET: netSpec } });
  if (process.env.SYNC_SERVER_LOG) fixture.stderr.pipe(createWriteStream(process.env.SYNC_SERVER_LOG));
  const lines = createInterface({ input: fixture.stdout });
  const ready = await new Promise((resolve, reject) => {
    lines.once("line", line => resolve(JSON.parse(line)));
    fixture.once("exit", () => reject(new Error(`fixture exited: ${fixture.output}`)));
  });

  const browser = await chromium.launch({ channel: "chromium", headless: true, ignoreDefaultArgs: ["--mute-audio"], args: ["--autoplay-policy=no-user-gesture-required", ...(process.platform === "darwin" ? ["--disable-audio-output"] : []), `--auto-select-tab-capture-source-by-title=${SOURCE_TITLE}`] });
  const viewerEngine = process.env.SYNC_ENGINE === "webkit" ? await webkit.launch({ headless: true }) : browser;
  try {
    const sharer = await device(browser, web, ready.url, "sync-sharer");
    let source = sharer;
    if (!process.env.SYNC_FAKE_TAB) {
      source = await sharer.context_.newPage();
      await source.route("**/share-sync-source", route => route.fulfill({ contentType: "text/html", body: sourcePage }));
      await source.goto(`${web}/share-sync-source`);
      await source.evaluate(() => window.__toneContext.resume());
      await sharer.bringToFront();
    }
    await joinDefault(sharer);
    await sharer.getByRole("button", { name: "Share screen", exact: true }).click();
    const dialog = sharer.getByRole("dialog", { name: "Share your screen" });
    await dialog.waitFor({ timeout: 20_000 });
    await dialog.getByLabel("Frame rate", { exact: true }).selectOption("60");
    await dialog.getByRole("button", { name: "Share", exact: true }).click();
    await sharer.getByRole("status", { name: "You are sharing your screen" }).waitFor({ timeout: 20_000 });

    const watching = [];
    for (const kind of VIEWERS) watching.push(await viewer(viewerEngine, web, ready.url, "sync-sharer", kind));
    const workerFail = process.env.SYNC_WORKER_FAIL ? await workerFailed(watching, process.env.SYNC_WORKER_FAIL) : null;
    let soundBack = null;
    if (process.env.SYNC_ADD) { await sleep(8_000); soundBack = await turnOnSound(sharer, watching); }
    if (process.env.SYNC_RETURN) {
      await sleep(8_000);
      await sharer.evaluate(async () => (await window.__gwarE2eImport("/src/state/screen.ts")).useScreen.getState().own.stream.getAudioTracks().forEach(track => track.stop()));
      await sleep(5_000);
      soundBack = await turnOnSound(sharer, watching);
    }
    await sleep(seconds * 1000);
    const bursts = await source.evaluate(() => window.__bursts);
    const report = { net: netName, spec: netSpec, bursts, soundBack, workerFail, viewers: {} };
    for (const page of watching) {
      if (page.pictureOnly) {
        await checkPictureOnly(page);
        report.viewers[page.kind] = { pictureOnly: true };
        console.log(JSON.stringify({ net: netName, viewer: page.kind, pictureOnly: true }));
        continue;
      }
      const record = await viewerRecord(page);
      if (process.env.SYNC_OFF) assert.deepEqual(baselineFaults(record.baseline), [], `the ${page.kind} viewer's baseline is not main`);
      const pairs = pairBursts(bursts, record.presented, record.onsets);
      report.viewers[page.kind] = { summary: offsetSummary(pairs), pairs, onsets: record.onsets, mixer: record.mixer, health: record.health, presented: record.presented, audible: record.audible, granted: record.granted, linkBacks: record.linkBacks, sheds: record.sheds, looks: record.looks, waits: record.waits };
      console.log(JSON.stringify({ net: netName, viewer: page.kind, ...offsetSummary(pairs), drawn: bursts.length, mixer: record.mixer }));
      console.log(pairs.map(pair => pair.offsetMs.toFixed(0)).join(" "));
    }
    // Stops the relay and returns what it did, so a run on a lossy harness is seen as one.
    report.relay = await Promise.race([new Promise(resolve => {
      lines.on("line", line => { const value = JSON.parse(line); if (value.event === "finish") resolve(value.network); });
      fixture.stdin.write("finish\n");
    }), sleep(3_000)]);
    if (report.relay) console.log(JSON.stringify({ sendErrors: report.relay.downlink_send_errors, down: report.relay.down }));
    if (process.env.SYNC_OUT) writeFileSync(process.env.SYNC_OUT, JSON.stringify(report, null, 1));
  } finally {
    await browser.close();
    if (viewerEngine !== browser) await viewerEngine.close();
    await stopProcesses(children);
    rmSync(scratch, { recursive: true, force: true });
  }
}

await run();
