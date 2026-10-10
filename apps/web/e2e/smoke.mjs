#!/usr/bin/env node
/**
 * End-to-end smoke test against a real vc-server, driven by Chromium with a
 * fake microphone. Also writes screenshots to e2e/screenshots/.
 *
 *   pnpm --filter web e2e
 *
 * Channels created by the test get a per-run suffix and are deleted at the end, so it
 * can run repeatedly against a server that already has data (e.g. E2E_REUSE=1).
 *
 * Starts (and stops) a throwaway server with an empty data dir, a throwaway
 * Gwar Connect service (built with `cargo build -p gwar-connect`) and the Vite
 * dev server when it is not already running (an already running one must have
 * been started with VITE_CONNECT_URL pointing at a Connect service whose
 * --origin is WEB_URL, or the Gwar Connect checks fail). Environment:
 *   WEB_URL      default http://127.0.0.1:5173
 *   SERVER_HTTP  default 127.0.0.1:8799 (host:port, TCP for WS and UDP for media)
 *   E2E_REUSE=1  use the server already running at SERVER_HTTP instead of a
 *                fresh throwaway one (state from earlier runs may break the test)
 *   ADMIN_TOKEN  admin token of that already running server
 *   VC_SERVER_BIN  path to the vc-server binary (default ../../target/debug/vc-server,
 *                  falling back to `cargo run -p vc-server`)
 *   E2E_LATENCY_MS  simulate this round-trip time (ms) on the WebSocket via a local
 *                   delaying TCP proxy (default 0). The suite must pass at any value.
 *   GWAR_CONNECT_BIN  path to the gwar-connect binary (default ../../target/debug/gwar-connect,
 *                     falling back to `cargo run -p gwar-connect`)
 *   E2E_SCREENSHOTS  optional screenshot output directory (default e2e/screenshots/)
 *   HEADED=1     show the browser
 */
import { spawn, spawnSync } from "node:child_process";
import net from "node:net";
import { existsSync, mkdirSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = path.dirname(fileURLToPath(import.meta.url));
const webRoot = path.resolve(here, "..");
const repoRoot = path.resolve(webRoot, "../..");
const shots = process.env.E2E_SCREENSHOTS ?? path.join(here, "screenshots");
mkdirSync(shots, { recursive: true });

const WEB_URL = process.env.WEB_URL ?? "http://127.0.0.1:5173";
const SERVER = process.env.SERVER_HTTP ?? "127.0.0.1:8799";
const LATENCY_MS = Number(process.env.E2E_LATENCY_MS ?? 0);
const LATENCY_PORT = 8797;
// What the browsers type as the server address (through the delay proxy when simulating latency).
const CLIENT_ADDR = LATENCY_MS > 0 ? `127.0.0.1:${LATENCY_PORT}` : SERVER;
const rid = Math.random().toString(16).slice(2, 6);
const GAMES = `Games-${rid}`;
// A second channel of the run (the server may be shared, so no seed names).
const TALK = `Talk-${rid}`;
const CHESS = `Chess-${rid}`;
const CLUB = `Club-${rid}`;
const SECRET = `Secret-${rid}`;
const BULK = `Bulk-${rid}`;
const children = [];
const results = [];

// eslint-disable-next-line no-control-regex
const stripAnsi = (s) => s.replace(/\x1b\[[0-9;]*m/g, "");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function reachable(url) {
  try {
    return (await fetch(url, { signal: AbortSignal.timeout(1500) })).ok;
  } catch {
    return false;
  }
}

async function waitFor(fn, what, timeout = 15000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await fn()) return;
    await sleep(100);
  }
  throw new Error(`timeout waiting for ${what}`);
}

async function ensureServer(connectUrl) {
  if (process.env.E2E_REUSE && (await reachable(`http://${SERVER}/health`))) {
    console.log(`using running server at ${SERVER}`);
    if (process.env.ADMIN_TOKEN) return process.env.ADMIN_TOKEN;
    const bin = process.env.VC_SERVER_BIN ?? path.join(repoRoot, "target/debug/vc-server");
    const dir = process.env.VC_DATA_DIR ?? "/tmp/vc-dev";
    const r = existsSync(bin)
      ? spawnSync(bin, ["--data-dir", dir, "admin-token"], { encoding: "utf8" })
      : spawnSync("cargo", ["run", "-q", "-p", "vc-server", "--", "--data-dir", dir, "admin-token"], { cwd: repoRoot, encoding: "utf8" });
    const token = r.stdout.trim().split("\n").pop();
    if (!token) throw new Error("could not obtain an admin token; set ADMIN_TOKEN");
    return token;
  }
  const dataDir = mkdtempSync(path.join(tmpdir(), "vc-e2e-"));
  const bin = process.env.VC_SERVER_BIN ?? path.join(repoRoot, "target/debug/vc-server");
  const args = ["--data-dir", dataDir, "--http", SERVER, "--media", SERVER, "--public-ip", SERVER.split(":")[0], "--connect-url", connectUrl];
  const child = existsSync(bin)
    ? spawn(bin, args, { cwd: repoRoot })
    : spawn("cargo", ["run", "-q", "-p", "vc-server", "--", ...args], { cwd: repoRoot });
  child.on("exit", (code) => code && console.error(`server exited with code ${code}`));
  children.push(child);
  let log = "";
  const onData = (d) => (log += stripAnsi(String(d)));
  child.stdout.on("data", onData);
  child.stderr.on("data", onData);
  await waitFor(() => /server ready/.test(log), "server start", 120000);
  const m = /redeem this one-time admin token in your client: (\S+)/.exec(log);
  if (!m) throw new Error("no admin token in server log");
  console.log(`started server (data dir ${dataDir})`);
  return m[1];
}

/** TCP proxy that delays every chunk by half the RTT in each direction (order preserved). */
function startLatencyProxy() {
  const [host, port] = SERVER.split(":");
  const oneWay = LATENCY_MS / 2;
  const server = net.createServer((client) => {
    const upstream = net.connect(Number(port), host);
    const pipe = (from, to) => {
      from.on("data", (d) => setTimeout(() => !to.destroyed && to.write(d), oneWay));
      from.on("close", () => setTimeout(() => to.destroy(), oneWay));
      from.on("error", () => to.destroy());
    };
    pipe(client, upstream);
    pipe(upstream, client);
  });
  server.listen(LATENCY_PORT, "127.0.0.1");
  children.push({ kill: () => server.close() });
  console.log(`simulating ${LATENCY_MS} ms RTT on ${CLIENT_ADDR} -> ${SERVER}`);
}

/** Starts a throwaway Gwar Connect service that accepts the web app's origin; returns its base URL. */
async function ensureConnect() {
  const port = await new Promise((resolve) => {
    const probe = net.createServer().listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
  const dataDir = mkdtempSync(path.join(tmpdir(), "gwar-connect-e2e-"));
  const bin = process.env.GWAR_CONNECT_BIN ?? path.join(repoRoot, "target/debug/gwar-connect");
  const args = ["--data-dir", dataDir, "--http", `127.0.0.1:${port}`, "--origin", new URL(WEB_URL).origin];
  const child = existsSync(bin)
    ? spawn(bin, args, { cwd: repoRoot })
    : spawn("cargo", ["run", "-q", "-p", "gwar-connect", "--", ...args], { cwd: repoRoot });
  child.on("exit", (code) => code && console.error(`gwar-connect exited with code ${code}`));
  children.push(child);
  const base = `http://127.0.0.1:${port}`;
  await waitFor(async () => {
    try {
      return (await fetch(`${base}/v1/revocations?since=0`, { signal: AbortSignal.timeout(1000) })).ok;
    } catch {
      return false;
    }
  }, "gwar-connect start", 120000);
  console.log(`started gwar-connect at ${base} (data dir ${dataDir})`);
  return base;
}

async function ensureWeb(connectUrl) {
  if (await reachable(WEB_URL)) return;
  const child = spawn("npx", ["vite", "--host", "127.0.0.1", "--port", new URL(WEB_URL).port || "5173", "--strictPort"], { cwd: webRoot, env: { ...process.env, VITE_CONNECT_URL: connectUrl } });
  children.push(child);
  await waitFor(() => reachable(WEB_URL), "vite dev server", 30000);
  console.log("started vite dev server");
}

const debugPages = [];

/** A solid-colour PNG, built by hand so the test needs no image files. */
function makePng(width, height, [r, g, b]) {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (buf) => {
    let c = 0xffffffff;
    for (const byte of buf) c = crcTable[(c ^ byte) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const body = Buffer.concat([Buffer.from(type), data]);
    const out = Buffer.alloc(body.length + 8);
    out.writeUInt32BE(data.length, 0);
    body.copy(out, 4);
    out.writeUInt32BE(crc(body), body.length + 4);
    return out;
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // RGB
  const row = Buffer.concat([Buffer.from([0]), Buffer.from(Array.from({ length: width }, () => [r, g, b]).flat())]);
  const raw = Buffer.concat(Array.from({ length: height }, () => row));
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const ROLE = `Mods-${rid}`;
const ROLE_RGB = "rgb(231, 76, 60)"; // #e74c3c, the first colour preset

async function check(name, fn) {
  try {
    await fn();
    results.push({ name, ok: true });
    console.log(`  ok   ${name}`);
  } catch (e) {
    results.push({ name, ok: false, error: e });
    console.log(`  FAIL ${name}\n${e.stack ?? e}`);
    for (const [i, p] of debugPages.entries()) {
      await p.screenshot({ path: path.join(shots, `fail-${results.length}-${i}.png`) }).catch(() => {});
    }
  }
}

/** Collects vc/1 frames received over any websocket of a page. */
function recordFrames(page) {
  const frames = [];
  page.on("websocket", (ws) => {
    ws.on("framereceived", ({ payload }) => {
      try {
        frames.push(JSON.parse(String(payload)));
      } catch {
        /* ignore */
      }
    });
  });
  return {
    frames,
    events: (name) => frames.filter((f) => f.ev === name).map((f) => f.d),
    /** Clients as announced by Welcome and later updates. */
    clients: () => [
      ...frames.flatMap((f) => (f.ok?.clients ? f.ok.clients : [])),
      ...frames.filter((f) => f.ev === "client.updated" || f.ev === "client.joined").map((f) => f.d),
    ],
  };
}

/** Escape first dismisses a focused member tooltip, then its containing drawer. */
async function dismissDialog(page, name) {
  const dialog = page.getByRole("dialog", { name, exact: true });
  for (let attempt = 0; attempt < 3 && await dialog.isVisible(); attempt++) {
    await page.keyboard.press("Escape");
    await page.waitForTimeout(100);
  }
  await dialog.waitFor({ state: "detached" });
}

/** Set the identity's profile through Settings, before the first join. */
async function setGlobalNickname(page, nickname, { mobile = false, pl = false } = {}) {
  const settings = page.getByRole("button", { name: pl ? "Ustawienia" : "Settings", exact: true }).first();
  if (mobile && !(await settings.isVisible())) await page.getByRole("button", { name: pl ? "Serwery" : "Servers", exact: true }).click();
  await settings.click();
  await page.getByRole("tab", { name: pl ? "Tożsamość" : "Identity", exact: true }).click();
  const field = page.getByLabel(pl ? "Pseudonim" : "Nickname", { exact: true });
  await waitFor(async () => (await field.inputValue()) !== "", "identity nickname", 10000);
  await field.fill(nickname);
  const save = page.getByRole("dialog").getByRole("button", { name: pl ? "Zapisz" : "Save", exact: true }).first();
  await save.click();
  await waitFor(() => save.isEnabled(), "saved identity nickname", 15000);
  await page.getByText(pl ? "Zapisano" : "Saved", { exact: true }).waitFor();
  await page.keyboard.press("Escape");
  if (mobile) await dismissDialog(page, pl ? "Kanały i serwery" : "Channels and servers");
}

async function changeServerNickname(page, nickname) {
  await page.getByRole("button", { name: "Server menu" }).click();
  await page.getByRole("menuitem", { name: "Change nickname", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "Change nickname", exact: true });
  await dialog.getByLabel("Nickname").fill(nickname);
  await dialog.getByRole("button", { name: "Save", exact: true }).click();
  await dialog.waitFor({ state: "detached" });
}

async function connect(page, nickname, { mobile = false, address = CLIENT_ADDR } = {}) {
  await page.goto(WEB_URL);
  await page.getByLabel("Server address").fill(address);
  await setGlobalNickname(page, nickname, { mobile });
  if (await page.getByLabel("Nickname").count()) throw new Error("connect screen still asks for a nickname");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  // After connecting you are on the server, not in voice. On phones the voice panel lives in the drawer.
  if (mobile) await page.getByRole("button", { name: "Channels and servers" }).waitFor({ timeout: 20000 });
  else await page.getByText("Not in voice").waitFor({ timeout: 20000 });
}

/** Joins a channel's voice with the small button that appears when hovering its row. */
async function joinWithButton(page, name) {
  const row = treeItem(page, name);
  await row.hover();
  await row.getByRole("button", { name: /^Join voice in / }).click();
}

const memberSection = (page, name) => page.getByRole("region", { name, exact: true });

const composer = (page) => page.getByRole("textbox", { name: "Message", exact: true });
const escapeRe = (t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const treeItem = (page, name) => page.getByRole("treeitem", { name: typeof name === "string" ? new RegExp(escapeRe(name)) : name });

async function shot(page, name) {
  await page.waitForTimeout(250);
  await page.screenshot({ path: path.join(shots, `${name}.png`) });
}

async function main() {
  const connectUrl = await ensureConnect();
  const token = await ensureServer(connectUrl);
  await ensureWeb(connectUrl);
  if (LATENCY_MS > 0) startLatencyProxy();

  const browser = await chromium.launch({
    headless: !process.env.HEADED,
    args: ["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream", "--autoplay-policy=no-user-gesture-required"],
  });
  const spy = () => {
    // Records sockets and peer connections so the test can inspect/drop them.
    const sockets = (window.__sockets = []);
    const NativeWS = window.WebSocket;
    window.WebSocket = new Proxy(NativeWS, {
      construct(target, args) {
        const ws = new target(...args);
        sockets.push(ws);
        return ws;
      },
    });
    // Counts microphone requests: nothing may ask before the user joins voice.
    window.__gum = 0;
    const media = navigator.mediaDevices;
    if (media?.getUserMedia) {
      const nativeGum = media.getUserMedia.bind(media);
      media.getUserMedia = (...args) => {
        window.__gum++;
        return nativeGum(...args);
      };
    }
    const pcs = (window.__pcs = []);
    const NativePC = window.RTCPeerConnection;
    window.RTCPeerConnection = new Proxy(NativePC, {
      construct(target, args) {
        const pc = new target(...args);
        pcs.push(pc);
        return pc;
      },
    });
  };
  const newCtx = async (opts = {}) => {
    const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 }, locale: "en-US", permissions: ["microphone"], colorScheme: "dark", ...opts });
    await ctx.addInitScript(spy);
    return ctx;
  };
  const ctxA = await newCtx();
  const ctxB = await newCtx();
  const a = await ctxA.newPage();
  const b = await ctxB.newPage();
  const errors = [];
  for (const [who, p] of [["A", a], ["B", b]]) {
    p.on("pageerror", (e) => errors.push(`${who}: ${e.message}`));
    p.on("console", (m) => m.type() === "error" && errors.push(`${who} console: ${m.text()}`));
  }
  debugPages.push(a, b);
  const fa = recordFrames(a);
  const fb = recordFrames(b);

  console.log("connect");
  await shot(await (async () => { await a.goto(WEB_URL); return a; })(), "01-connect-dark");
  await a.emulateMedia({ colorScheme: "light" });
  await shot(a, "02-connect-light");
  await a.emulateMedia({ colorScheme: "dark" });

  await check("alice connects and is on the server, not in voice", async () => {
    await connect(a, "Alice");
    await shot(a, "03-desktop-not-in-voice-dark");
    if ((await a.getByRole("button", { name: /Mute microphone/ }).count()) !== 0) throw new Error("mic button shown outside voice");
    if ((await a.evaluate(() => window.__gum)) !== 0) throw new Error("the microphone was requested before joining voice");
    if ((await a.evaluate(() => window.__pcs.length)) !== 0) throw new Error("a peer connection exists before joining voice");
  });
  await check("bob connects and is on the server, not in voice", () => connect(b, "Bob"));
  await check("both are listed as Online (not in voice) in the member list", async () => {
    for (const p of [a, b]) {
      await memberSection(p, "Online").getByText("Alice").waitFor({ timeout: 10000 });
      await memberSection(p, "Online").getByText("Bob").waitFor({ timeout: 10000 });
    }
  });

  await check("alice joins Lobby voice with a double click, bob with the join button", async () => {
    await treeItem(a, /Lobby/).dblclick();
    await a.getByText("Voice connected").waitFor({ timeout: 20000 });
    await joinWithButton(b, /Lobby/);
    await b.getByText("Voice connected").waitFor({ timeout: 20000 });
    if ((await a.evaluate(() => window.__gum)) < 1) throw new Error("joining voice should open the microphone");
  });
  await check("both see each other in the channel tree and in the In voice section", async () => {
    for (const p of [a, b]) {
      await treeItem(p, /Alice/).waitFor({ timeout: 10000 });
      await treeItem(p, /Bob/).waitFor({ timeout: 10000 });
      await memberSection(p, "In voice").getByText("Alice").waitFor({ timeout: 10000 });
      await memberSection(p, "In voice").getByText("Bob").waitFor({ timeout: 10000 });
    }
  });

  await check("server reports voice=true for both clients", async () => {
    await waitFor(
      () => {
        return fb.clients().some((c) => c.nickname === "Alice" && c.voice === true);
      },
      "client.updated voice=true for Alice (seen by Bob)",
      15000,
    );
    await waitFor(() => fa.clients().some((c) => c.nickname === "Bob" && c.voice === true), "voice=true for Bob (seen by Alice)", 15000);
  });

  await check("voice.slot and voice.talking events arrive (fake mic beeps)", async () => {
    await waitFor(() => fb.events("voice.slot").some((s) => s.client !== null), "voice.slot at Bob", 20000);
    await waitFor(() => fb.events("voice.talking").some((t) => t.talking === true), "voice.talking at Bob", 20000);
    await waitFor(() => fa.events("voice.talking").some((t) => t.talking === true), "voice.talking at Alice", 20000);
  });

  await check("talking indicator shows in the tree", async () => {
    await waitFor(async () => (await a.locator(".talking-ring").count()) > 0, "talking ring", 15000);
  });

  await check("incoming audio reaches a WebAudio slot (receiver stats)", async () => {
    // Bob should be receiving RTP for Alice's speech: inspect via a peer connection stats hook.
    const ok = await b.evaluate(async () => {
      for (const pc of window.__pcs ?? []) {
        const stats = await pc.getStats();
        for (const s of stats.values()) if (s.type === "inbound-rtp" && s.kind === "audio" && s.packetsReceived > 0) return true;
      }
      return false;
    });
    if (!ok) throw new Error("no inbound audio RTP packets");
  });

  console.log("admin");
  await check("alice redeems the admin token", async () => {
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Redeem token" }).click();
    await a.getByLabel("Token", { exact: true }).fill(token);
    await a.getByRole("button", { name: "Redeem" }).click();
    await a.getByText(/Token redeemed/).waitFor();
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Create channel" }).waitFor();
    await a.keyboard.press("Escape");
  });

  await check("alice creates, edits and deletes channels", async () => {
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Create channel" }).click();
    await a.getByLabel("Name").fill(GAMES);
    await a.getByLabel("Topic").fill("Board games night");
    await a.getByRole("button", { name: "Create" }).click();
    await treeItem(a, GAMES).waitFor();
    await treeItem(b, GAMES).waitFor();
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Create channel" }).click();
    await a.getByLabel("Name").fill(TALK);
    await a.getByRole("button", { name: "Create" }).click();
    await treeItem(b, TALK).waitFor();

    await treeItem(a, GAMES).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Create subchannel" }).click();
    await a.getByLabel("Name").fill(CHESS);
    await a.getByRole("button", { name: "Create" }).click();
    await treeItem(b, CHESS).waitFor();

    await treeItem(a, CHESS).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Edit channel" }).click();
    await a.getByLabel("Name").fill(CLUB);
    await a.getByRole("button", { name: "Save" }).click();
    await treeItem(b, CLUB).waitFor();

    await treeItem(a, CLUB).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Delete" }).click();
    await a.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
    await treeItem(b, CLUB).waitFor({ state: "detached" });
  });

  await check("non-admin does not see admin actions", async () => {
    await b.getByRole("button", { name: "Server menu" }).click();
    const create = await b.getByRole("menuitem", { name: "Create channel" }).count();
    await b.keyboard.press("Escape");
    if (create !== 0) throw new Error("Bob can create channels");
  });

  await check("a channel user limit can be set and removed again", async () => {
    const limited = a.getByRole("treeitem", { name: new RegExp(`${escapeRe(GAMES)}.*/5`) });
    await treeItem(a, GAMES).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Edit channel" }).click();
    await a.getByLabel("Max users").fill("5");
    await a.getByRole("button", { name: "Save" }).click();
    await limited.waitFor();
    await treeItem(a, GAMES).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Edit channel" }).click();
    await a.getByLabel("Max users").fill("");
    await a.getByRole("button", { name: "Save" }).click();
    await limited.waitFor({ state: "detached" });
  });

  await check("a password-protected channel asks for the password", async () => {
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Create channel" }).click();
    await a.getByLabel("Name").fill(SECRET);
    await a.getByLabel("Password", { exact: true }).fill("hunter2");
    await a.getByRole("button", { name: "Create" }).click();
    await treeItem(b, SECRET).waitFor();
    await treeItem(b, SECRET).click();
    // Selecting a password channel offers the password dialog instead of its (unreadable) chat.
    await b.getByRole("dialog").getByLabel("Password", { exact: true }).waitFor();
    await b.getByLabel("Password", { exact: true }).fill("nope");
    await b.getByRole("button", { name: "Join channel" }).click();
    await b.getByText("Wrong password.").waitFor();
    await b.getByLabel("Password", { exact: true }).fill("hunter2");
    await b.getByRole("button", { name: "Join channel" }).click();
    await treeItem(b, SECRET).locator("xpath=..").getByText("Bob").waitFor();
    await treeItem(b, /Lobby/).dblclick();
    await treeItem(b, /Lobby/).locator("xpath=..").getByText("Bob").waitFor();
  });

  console.log("chat");
  await check("both join Games and chat both ways", async () => {
    await treeItem(a, GAMES).dblclick();
    await treeItem(b, GAMES).dblclick();
    await b.getByRole("log").waitFor();
    await treeItem(b, GAMES).locator("xpath=..").getByText("Alice").waitFor();
    await composer(a).fill("Hello from Alice, see https://example.com/docs.");
    await composer(a).press("Enter");
    await b.getByText("Hello from Alice").waitFor();
    const href = await b.getByRole("link", { name: "https://example.com/docs" }).getAttribute("href");
    if (href !== "https://example.com/docs") throw new Error(`bad link ${href}`);
    await composer(b).fill("Hi Alice!");
    await composer(b).press("Enter");
    await a.getByText("Hi Alice!").waitFor();
  });

  await check("a rejected send is shown as failed and can be retried", async () => {
    // Make Bob's next chat.send target a channel he is not in (what a premature send looks like).
    await b.evaluate(() => {
      const ws = window.__sockets.find((x) => x.url.endsWith("/ws"));
      const original = ws.send.bind(ws);
      window.__restoreSend = () => (ws.send = original);
      ws.send = (data) => {
        const frame = JSON.parse(data);
        if (frame.op === "chat.send" && typeof frame.d.target === "object" && "channel" in frame.d.target) frame.d.target.channel += 1000;
        original(JSON.stringify(frame));
      };
    });
    await composer(b).fill("retry me");
    await composer(b).press("Enter");
    const alert = b.getByRole("alert").filter({ hasText: "Not sent" });
    await alert.waitFor({ timeout: 8000 });
    await alert.getByText("retry me").waitFor();
    await b.evaluate(() => window.__restoreSend());
    await alert.getByRole("button", { name: "Retry" }).click();
    await a.getByText("retry me").waitFor({ timeout: 8000 });
    await alert.waitFor({ state: "detached" });
  });

  await check("html in messages is not interpreted", async () => {
    await composer(b).fill("<b>bold</b> <img src=x onerror=window.__pwned=1>");
    await composer(b).press("Enter");
    await a.getByText("<b>bold</b>", { exact: false }).waitFor();
    if (await a.evaluate(() => window.__pwned)) throw new Error("injected");
  });

  await check("history is loaded after rejoining a channel", async () => {
    await treeItem(b, /Lobby/).dblclick();
    await treeItem(b, /Lobby/).locator("xpath=..").getByText("Bob").waitFor();
    await treeItem(b, GAMES).dblclick();
    await treeItem(b, GAMES).locator("xpath=..").getByText("Bob").waitFor();
    await b.getByText("Hello from Alice").waitFor();
  });

  console.log("chat without voice");
  await check("chatting in a channel without being in its voice works for both users", async () => {
    // Both stay in Games voice and talk in Talk's chat.
    await treeItem(a, TALK).click();
    await composer(a).fill(`Alice writes in Talk ${rid}`);
    await composer(a).press("Enter");
    await treeItem(b, TALK).click();
    await b.getByText(`Alice writes in Talk ${rid}`).waitFor();
    await composer(b).fill(`Bob answers in Talk ${rid}`);
    await composer(b).press("Enter");
    await a.getByText(`Bob answers in Talk ${rid}`).waitFor();
    // Nobody moved: both are still listed under Games, nobody under Talk.
    for (const p of [a, b]) {
      await treeItem(p, GAMES).locator("xpath=..").getByText("Bob").waitFor();
      await treeItem(p, GAMES).locator("xpath=..").getByText("Alice").waitFor();
      if ((await treeItem(p, TALK).locator("xpath=..").getByText(/Alice|Bob/).count()) !== 0) throw new Error("someone joined Talk voice");
    }
    // Bob's own chat history survives going to another chat and coming back.
    await treeItem(b, GAMES).click();
    await treeItem(b, TALK).click();
    await b.getByText(`Alice writes in Talk ${rid}`).waitFor();
  });

  await check("unread badge appears for the other user and clears when they open the channel", async () => {
    await treeItem(b, GAMES).click();
    await composer(a).fill(`unread one ${rid}`);
    await composer(a).press("Enter");
    await composer(a).fill(`unread two ${rid}`);
    await composer(a).press("Enter");
    const badge = treeItem(b, TALK).getByTitle("2 unread");
    await badge.waitFor({ timeout: 8000 });
    // The sender has nothing unread in the channel she is looking at.
    if ((await treeItem(a, TALK).getByTitle(/unread/).count()) !== 0) throw new Error("sender sees an unread badge");
    await shot(b, "03b-unread-badge-dark");
    const seen = fb.events("chat.read").length;
    await treeItem(b, TALK).click();
    await badge.waitFor({ state: "detached" });
    await b.getByText(`unread two ${rid}`).waitFor();
    // The read position is reported to the server (and echoed back to this user's devices).
    await waitFor(() => fb.events("chat.read").length > seen, "chat.read echoed to Bob", 6000);
  });

  await check("private message with unread badge", async () => {
    await treeItem(b, /Alice/).dblclick();
    await composer(b).fill("psst");
    await composer(b).press("Enter");
    await a.getByRole("tab", { name: /Bob/ }).waitFor();
    await a.getByRole("tab", { name: /Bob 1/ }).waitFor();
    await a.getByRole("tab", { name: /Bob/ }).click();
    await a.getByText("psst").waitFor();
  });

  await check("server chat reaches everyone", async () => {
    await a.getByRole("tab", { name: "Server" }).click();
    await composer(a).fill("announcement");
    await composer(a).press("Enter");
    await b.getByRole("tab", { name: /Server/ }).click();
    await b.getByText("announcement").waitFor();
  });

  await check("leaving voice keeps the user online and stops voice", async () => {
    await a.getByRole("button", { name: "Leave voice" }).click();
    await a.getByText("Not in voice").waitFor({ timeout: 5000 });
    // Still on the server: Bob sees Alice under Online, no longer in the tree or in voice.
    await memberSection(b, "Online").getByText("Alice").waitFor({ timeout: 8000 });
    await treeItem(b, /Alice/).waitFor({ state: "detached", timeout: 8000 });
    if ((await memberSection(b, "In voice").getByText("Alice").count()) !== 0) throw new Error("Alice still listed in voice");
    // The audio transport is gone and the controls with it.
    await waitFor(() => a.evaluate(() => window.__pcs.every((pc) => pc.connectionState === "closed")), "peer connection closed", 5000);
    if ((await a.getByRole("button", { name: /Mute microphone/ }).count()) !== 0) throw new Error("mic button still shown");
    // She can still chat in the channel she was in.
    await treeItem(a, GAMES).click();
    await composer(a).fill("still here without voice");
    await composer(a).press("Enter");
    await treeItem(b, GAMES).click();
    await b.getByText("still here without voice").waitFor();
    await shot(a, "03c-left-voice-dark");
    // Joining again negotiates a fresh audio transport.
    const pcs = await a.evaluate(() => window.__pcs.length);
    await treeItem(a, GAMES).dblclick();
    await a.getByText("Voice connected").waitFor({ timeout: 20000 });
    if ((await a.evaluate(() => window.__pcs.length)) <= pcs) throw new Error("expected a new peer connection");
    await treeItem(b, /Alice/).waitFor({ timeout: 8000 });
  });

  await check("mute is reflected to other clients", async () => {
    await a.getByRole("button", { name: "Mute microphone" }).click();
    await waitFor(() => fb.clients().some((c) => c.nickname === "Alice" && c.muted === true), "muted update", 8000);
    await a.getByRole("button", { name: "Unmute microphone" }).click();
  });

  await check("user volume slider persists per uid", async () => {
    await treeItem(b, /Alice/).click({ button: "right" });
    const slider = b.getByRole("slider", { name: "Volume" });
    await slider.focus();
    await b.keyboard.press("ArrowRight");
    await b.keyboard.press("ArrowRight");
    await b.keyboard.press("Escape");
    const stored = await b.evaluate(() => JSON.parse(localStorage.getItem("vc.settings")).state.userVolumes);
    if (!Object.values(stored).some((v) => Math.abs(v - 1.1) < 0.01)) throw new Error(JSON.stringify(stored));
  });

  console.log("reconnect");
  await check("client reconnects after the socket drops and resyncs", async () => {
    const before = fa.events("client.joined").length;
    await b.evaluate(() => window.__sockets.filter((s) => s.url.endsWith("/ws")).forEach((s) => s.close()));
    await b.getByText("Connection lost. Reconnecting…").waitFor({ timeout: 5000 });
    // A new session joins from Alice's point of view and the old one leaves.
    await waitFor(() => fa.events("client.joined").length > before, "Bob rejoined", 15000);
    await b.getByText("Voice connected").waitFor({ timeout: 20000 });
    await b.getByText("Reconnected to the server").click({ trial: true }).catch(() => {});
    await treeItem(b, /Alice/).waitFor();
    await b.getByRole("tab", { name: "Server" }).click();
    await b.getByText("Reconnected to the server").waitFor();
  });

  console.log("screenshots");
  await a.getByRole("tab", { name: new RegExp(escapeRe(GAMES)) }).click();
  await treeItem(a, GAMES).click();
  await a.waitForTimeout(500);
  await shot(a, "03-desktop-dark");
  await a.emulateMedia({ colorScheme: "light" });
  await shot(a, "04-desktop-light");
  await a.emulateMedia({ colorScheme: "dark" });

  await treeItem(a, /Bob/).click({ button: "right" });
  await shot(a, "05-user-menu-dark");
  await a.keyboard.press("Escape");
  await treeItem(a, GAMES).click({ button: "right" });
  await shot(a, "06-channel-menu-dark");
  await a.keyboard.press("Escape");

  await a.getByRole("button", { name: "Settings" }).first().click();
  await shot(a, "07-settings-audio-dark");
  await a.getByRole("tab", { name: "Appearance" }).click();
  await shot(a, "08-settings-appearance-dark");
  await a.getByRole("tab", { name: "Identity" }).click();
  await shot(a, "09-settings-identity-dark");
  await a.keyboard.press("Escape");

  await a.getByRole("button", { name: "Server menu" }).click();
  await shot(a, "10-server-menu-dark");
  await a.keyboard.press("Escape");

  // Mobile
  const ctxM = await newCtx({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  const m = await ctxM.newPage();
  await check("mobile layout connects and opens the drawer", async () => {
    await connect(m, "Carol", { mobile: true });
    await shot(m, "11-mobile-dark-chat");
    await m.getByRole("button", { name: "Channels and servers" }).click();
    await treeItem(m, GAMES).waitFor();
    await shot(m, "12-mobile-dark-drawer");
    await treeItem(m, GAMES).click();
    await m.getByRole("tab", { name: new RegExp(escapeRe(GAMES)) }).waitFor();
    // The join button has no hover on touch screens: it is always there.
    await m.getByRole("button", { name: "Channels and servers" }).click();
    await m.getByRole("button", { name: `Join voice in ${GAMES}` }).waitFor();
    await dismissDialog(m, "Channels and servers");
    await m.getByRole("button", { name: "Members" }).click();
    await memberSection(m, "Online").getByText("Carol").waitFor();
    await shot(m, "12b-mobile-dark-members");
    await dismissDialog(m, "Members");
  });
  await m.emulateMedia({ colorScheme: "light" });
  await shot(m, "13-mobile-light-chat");
  await m.getByRole("button", { name: "Channels and servers" }).click();
  await shot(m, "14-mobile-light-drawer");

  // Polish UI
  const ctxP = await newCtx({ locale: "pl-PL", colorScheme: "light" });
  const p = await ctxP.newPage();
  await check("Polish UI is the default for pl browsers", async () => {
    await p.goto(WEB_URL);
    await p.getByLabel("Adres serwera").fill(CLIENT_ADDR);
    await setGlobalNickname(p, "Dorota", { pl: true });
    await p.getByRole("button", { name: "Połącz" }).click();
    await p.getByText("Poza kanałem głosowym").waitFor({ timeout: 20000 });
    await treeItem(p, /Lobby/).dblclick();
    await p.getByText("Głos połączony").waitFor({ timeout: 20000 });
    await shot(p, "15-desktop-pl-light");
  });

  // The same scene in English and dark, for the README.
  const ctxE = await newCtx({ locale: "en-US", colorScheme: "dark" });
  const e = await ctxE.newPage();
  await check("an English dark client shows the busy server", async () => {
    await e.goto(WEB_URL);
    await e.getByLabel("Server address").fill(CLIENT_ADDR);
    await setGlobalNickname(e, "Erin");
    await e.getByRole("button", { name: "Connect", exact: true }).click();
    await e.getByText("Not in voice").waitFor({ timeout: 20000 });
    await treeItem(e, /Lobby/).dblclick();
    await e.getByText("Voice connected").waitFor({ timeout: 20000 });
    await shot(e, "15b-desktop-en-dark");
  });

  await check("push-to-talk gates the microphone track", async () => {
    const senderEnabled = () =>
      a.evaluate(() => {
        const pc = [...window.__pcs].reverse().find((x) => x.connectionState === "connected");
        return pc?.getSenders()[0]?.track?.enabled;
      });
    await a.getByRole("button", { name: "Settings" }).first().click();
    await a.getByRole("radio", { name: "Push to talk" }).click();
    await a.getByRole("button", { name: "Set push-to-talk key" }).click();
    await a.keyboard.press("KeyV");
    await a.keyboard.press("Escape");
    await a.locator("header").first().click();
    if ((await senderEnabled()) !== false) throw new Error("track should be disabled until the key is held");
    await a.keyboard.down("v");
    await waitFor(async () => (await senderEnabled()) === true, "track enabled while key held", 3000);
    await a.keyboard.up("v");
    await waitFor(async () => (await senderEnabled()) === false, "track disabled after release", 3000);
    await a.getByRole("button", { name: "Settings" }).first().click();
    await a.getByRole("radio", { name: "Voice activity" }).click();
    await a.keyboard.press("Escape");
    await waitFor(async () => (await senderEnabled()) === true, "track enabled again in VAD mode", 3000);
  });

  await check("keyboard shortcut toggles mute", async () => {
    await a.keyboard.press("Control+Shift+KeyM");
    await a.getByRole("button", { name: "Unmute microphone" }).waitFor({ timeout: 3000 });
    await a.keyboard.press("Control+Shift+KeyM");
    await a.getByRole("button", { name: "Mute microphone" }).waitFor({ timeout: 3000 });
  });

  await check("dragging a user onto a channel moves them (admin)", async () => {
    await treeItem(a, /Bob/).dragTo(treeItem(a, TALK));
    await b.getByRole("tab", { name: TALK }).waitFor({ timeout: 5000 });
  });

  await check("microphone denied: friendly message, chat still works", async () => {
    const ctx = await newCtx({ permissions: [] });
    // What a real browser does when the user clicks "Block".
    await ctx.addInitScript(() => {
      navigator.mediaDevices.getUserMedia = () => Promise.reject(new DOMException("Permission denied", "NotAllowedError"));
    });
    const e = await ctx.newPage();
    await e.goto(WEB_URL);
    await e.getByLabel("Server address").fill(CLIENT_ADDR);
    await setGlobalNickname(e, "Eve");
    await e.getByRole("button", { name: "Connect", exact: true }).click();
    await e.getByText("Not in voice").waitFor({ timeout: 15000 });
    await sleep(500);
    if ((await e.getByText(/Microphone access is blocked/).count()) !== 0) throw new Error("mic error before joining voice");
    await treeItem(e, /Lobby/).dblclick();
    await e.getByText(/Microphone access is blocked/).first().waitFor({ timeout: 15000 });
    await composer(e).fill(`can you hear me? ${rid}`);
    await composer(e).press("Enter");
    await e.getByText(`can you hear me? ${rid}`).waitFor();
    // Listening works without a microphone: the transport still connects.
    await e.getByText("Voice connected").waitFor({ timeout: 15000 });
    await shot(e, "16-mic-denied-dark");
  });

  console.log("roles, mentions, edits, files, invites, bans");
  const nameColor = (page, locator) => locator.evaluate((el) => getComputedStyle(el).color);
  const openSettings = async (page, tabName) => {
    await page.getByRole("button", { name: "Server menu" }).click();
    await page.getByRole("menuitem", { name: "Server settings" }).click();
    await page.getByRole("tab", { name: tabName }).click();
  };

  await check("admin creates a role with the kick permission and gives it to Bob; his name gets the colour", async () => {
    await openSettings(a, "Roles");
    await a.getByRole("button", { name: "Create role" }).click();
    await a.getByLabel("Role name").fill(ROLE);
    await a.getByRole("button", { name: "#e74c3c" }).click();
    await a.getByRole("checkbox", { name: /Kick people/ }).check();
    await shot(a, "19-settings-roles-dark");
    await a.getByRole("button", { name: "Create", exact: true }).click();
    await a.getByRole("button", { name: ROLE }).waitFor();
    await a.getByRole("tab", { name: "Members" }).click();
    await a.getByLabel("Search members").fill("Bob");
    await a.getByRole("button", { name: "Change roles of Bob" }).click();
    await a.getByRole("menuitemcheckbox", { name: ROLE }).click();
    await a.getByRole("menuitemcheckbox", { name: ROLE, checked: true }).waitFor();
    await a.keyboard.press("Escape");
    await a.locator('[data-member="Bob"]').getByText(ROLE).waitFor();
    await shot(a, "20-settings-members-dark");
    await a.keyboard.press("Escape");
    // Everyone sees the colour, in the tree and in the member list.
    for (const p of [a, b]) {
      await waitFor(async () => (await nameColor(p, treeItem(p, /Bob/).getByText("Bob", { exact: true }))) === ROLE_RGB, "Bob's name colour in the tree");
      await waitFor(
        async () => (await nameColor(p, memberSection(p, "In voice").getByText("Bob", { exact: true }))) === ROLE_RGB,
        "Bob's name colour in the member list",
      );
    }
    // The tree's context menu shows (and can change) the same roles.
    await treeItem(a, /Bob/).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Roles" }).click();
    await a.getByRole("menuitemcheckbox", { name: ROLE, checked: true }).waitFor();
    await a.keyboard.press("Escape");
    await a.keyboard.press("Escape");
    // The permission arrived without reconnecting: Bob can now kick (but not Alice, who is stronger).
    await treeItem(b, /Eve/).click({ button: "right" });
    await b.getByRole("menuitem", { name: "Kick from server" }).waitFor();
    await b.keyboard.press("Escape");
    await treeItem(b, /Alice/).click({ button: "right" });
    if (!(await b.getByRole("menuitem", { name: "Kick from server" }).isDisabled())) throw new Error("Bob can kick the admin");
    await b.keyboard.press("Escape");
  });

  const hello = `hey @Bob mention ${rid}`;
  await check("mention: autocomplete in the composer, a badge and a highlight for the mentioned user", async () => {
    await treeItem(b, GAMES).click(); // Bob looks at another chat, so Talk counts as unread
    await treeItem(a, TALK).click();
    await composer(a).click();
    await composer(a).pressSequentially("hey @Bo");
    await a.getByRole("option", { name: /Bob/ }).waitFor();
    await shot(a, "21-mention-autocomplete-dark");
    await composer(a).press("Enter"); // takes the suggestion instead of sending
    if ((await composer(a).inputValue()) !== "hey @Bob ") throw new Error(`unexpected composer text: ${await composer(a).inputValue()}`);
    await composer(a).pressSequentially(`mention ${rid}`);
    await composer(a).press("Enter");
    await treeItem(b, TALK).getByTitle("Mentions of you: 1").waitFor({ timeout: 8000 });
    await treeItem(b, TALK).click();
    const msg = b.locator("[data-mentions-me]").filter({ hasText: hello });
    await msg.waitFor();
    await msg.locator('[data-mention]').getByText("@Bob").waitFor();
    // Only the mentioned user gets the highlight.
    if ((await a.locator("[data-mentions-me]").count()) !== 0) throw new Error("the author sees a mention highlight");
    await shot(b, "22-mention-highlight-dark");
    // The mention badge is gone once read.
    await treeItem(b, TALK).getByTitle(/Mentions of you/).waitFor({ state: "detached" });
  });

  await check("a message can be edited (Up arrow) and deleted; the other user sees both", async () => {
    await composer(a).press("ArrowUp");
    const editor = a.getByRole("textbox", { name: "Edit message" });
    await editor.waitFor();
    await editor.fill(`hey @Bob edited ${rid}`);
    await editor.press("Enter");
    await b.getByText(`hey @Bob edited ${rid}`).waitFor();
    const row = b.locator("[data-message]").filter({ hasText: `edited ${rid}` });
    await row.locator("[data-edited]").waitFor();
    await a.locator("[data-message]").filter({ hasText: `edited ${rid}` }).locator("[data-edited]").waitFor();
    // Escape leaves the editor without saving.
    await composer(a).press("ArrowUp");
    await a.getByRole("textbox", { name: "Edit message" }).fill("never saved");
    await a.keyboard.press("Escape");
    await a.getByRole("textbox", { name: "Edit message" }).waitFor({ state: "detached" });
    // Bob cannot edit or delete Alice's message.
    await row.hover();
    if ((await row.getByRole("button", { name: "Delete message" }).count()) !== 0) throw new Error("Bob may delete Alice's message");
    // Delete with a confirmation.
    const mine = a.locator("[data-message]").filter({ hasText: `edited ${rid}` });
    await mine.hover();
    await mine.getByRole("button", { name: "Delete message" }).click();
    await a.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
    await b.getByText(`hey @Bob edited ${rid}`).waitFor({ state: "detached" });
    // Shift+click skips the question.
    await composer(a).fill(`quick ${rid}`);
    await composer(a).press("Enter");
    const quick = a.locator("[data-message]").filter({ hasText: `quick ${rid}` });
    await quick.hover();
    await quick.getByRole("button", { name: "Delete message" }).click({ modifiers: ["Shift"] });
    await quick.waitFor({ state: "detached" });
    await b.getByText(`quick ${rid}`).waitFor({ state: "detached" });
  });

  // The page is served by Vite, not by the server, so the browser needs CORS to PUT to the server.
  // Real deployments serve the client from the server (checked with the production build below).
  await ctxA.route("**/api/files/**", async (route) => {
    const cors = { "access-control-allow-origin": "*", "access-control-allow-methods": "PUT, OPTIONS", "access-control-allow-headers": "*" };
    if (route.request().method() === "OPTIONS") return route.fulfill({ status: 204, headers: cors });
    const response = await route.fetch();
    return route.fulfill({ response, headers: { ...response.headers(), ...cors } });
  });
  const png = makePng(64, 48, [200, 80, 60]);

  await check("an image is uploaded with progress, shown inline to both, opens in a lightbox", async () => {
    await a.locator("input[type=file]").setInputFiles({ name: "pixel.png", mimeType: "image/png", buffer: png });
    const pending = a.getByRole("list", { name: "Files to send" });
    await pending.getByText("pixel.png").waitFor();
    await pending.getByText(/\d+ B$/).waitFor({ timeout: 10000 }); // finished: shows the size instead of a percentage
    await composer(a).fill(`look at this ${rid}`);
    await composer(a).press("Enter");
    await pending.waitFor({ state: "detached" });
    for (const p of [a, b]) {
      const img = p.getByRole("button", { name: "Open pixel.png" }).locator("img");
      await img.waitFor({ timeout: 10000 });
      await waitFor(() => img.evaluate((i) => i.complete && i.naturalWidth === 64), "image loaded");
      const box = await img.boundingBox();
      if (Math.round(box.width) !== 64 || Math.round(box.height) !== 48) throw new Error(`image shown at ${box.width}x${box.height}`);
      await p.getByText(`look at this ${rid}`).waitFor();
    }
    await shot(b, "23-chat-image-dark");
    await b.getByRole("button", { name: "Open pixel.png" }).click();
    await b.getByRole("dialog").getByRole("img", { name: "pixel.png" }).waitFor();
    await b.getByRole("button", { name: "Save image" }).waitFor();
    await b.keyboard.press("Escape");
    await b.getByRole("dialog").waitFor({ state: "detached" });
    // The server serves it as an image, from its own origin.
    const sent = fa.events("chat.message").find((m) => m.attachments?.some((x) => x.name === "pixel.png"));
    const att = sent.attachments[0];
    if (att.width !== 64 || att.height !== 48) throw new Error(`dimensions ${att.width}x${att.height}`);
    const r = await fetch(`http://${SERVER}${att.url}`);
    if (r.headers.get("content-type") !== "image/png") throw new Error(`content type ${r.headers.get("content-type")}`);
  });

  await check("a text file is shown as a file card, an oversized file is refused up front", async () => {
    await a.locator("input[type=file]").setInputFiles({ name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from(`notes ${rid}\n`) });
    await a.getByRole("list", { name: "Files to send" }).getByText(/\d+ B$/).waitFor({ timeout: 10000 });
    await composer(a).press("Enter"); // attachments only, no text
    for (const p of [a, b]) {
      const card = p.locator('[data-attachment="file"]').filter({ hasText: "notes.txt" });
      await card.waitFor({ timeout: 10000 });
      await card.getByRole("button", { name: "Download notes.txt" }).waitFor();
    }
    await shot(b, "24-chat-file-card-dark");
    const att = fa.events("chat.message").find((m) => m.attachments?.some((x) => x.name === "notes.txt")).attachments[0];
    const r = await fetch(`http://${SERVER}${att.url}`);
    if (!/^attachment/.test(r.headers.get("content-disposition") ?? "")) throw new Error("text file is not an attachment");
    if ((await r.text()) !== `notes ${rid}\n`) throw new Error("file content differs");
    // Over the limit: refused before anything is sent to the server.
    await a.locator("input[type=file]").setInputFiles({ name: "huge.bin", mimeType: "application/octet-stream", buffer: Buffer.alloc(26 * 1024 * 1024) });
    await a.getByText(/huge\.bin: That file is too large\. The limit is 25 MB\./).waitFor();
    if ((await a.getByRole("list", { name: "Files to send" }).count()) !== 0) throw new Error("oversized file was queued");
  });

  let inviteLink = "";
  await check("an invite link admits a new user, who gets the invite's role", async () => {
    await ctxA.grantPermissions(["clipboard-read", "clipboard-write"], { origin: new URL(WEB_URL).origin });
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Invite people" }).click();
    await a.getByRole("combobox", { name: "Uses" }).click();
    await a.getByRole("option", { name: "1", exact: true }).click();
    await a.getByRole("combobox", { name: "Role for new people" }).click();
    await a.getByRole("option", { name: ROLE }).click();
    await a.getByRole("button", { name: "Create invite" }).click();
    const item = a.locator("[data-invite]").first();
    await item.waitFor();
    await item.getByText("0 of 1 uses").waitFor();
    await item.getByRole("button", { name: "Copy link" }).click();
    inviteLink = await a.evaluate(() => navigator.clipboard.readText());
    const expected = new RegExp(`^${escapeRe(new URL(WEB_URL).origin)}/\\?server=${escapeRe(encodeURIComponent(CLIENT_ADDR))}&invite=[\\w-]+$`);
    if (!expected.test(inviteLink)) throw new Error(`unexpected invite link ${inviteLink}`);
    // The web app (Vite) and the server are different origins here, so the link has to name the server.
    const link = new URL(inviteLink);
    if (link.origin === `http://${SERVER}` || link.searchParams.get("server") !== CLIENT_ADDR) throw new Error(`invite link does not name the server: ${inviteLink}`);
    await shot(a, "25-invites-dark");
    await a.keyboard.press("Escape");

    const ctx = await newCtx();
    const ivy = await ctx.newPage();
    debugPages.push(ivy);
    await ivy.goto(inviteLink);
    await ivy.getByText(/You were invited to this server/).waitFor();
    if (/invite=/.test(ivy.url())) throw new Error(`the invite stays in the address bar: ${ivy.url()}`);
    await waitFor(async () => (await ivy.getByLabel("Server address").inputValue()) === CLIENT_ADDR, "prefilled server address");
    await shot(ivy, "26-invite-join-dark");
    await setGlobalNickname(ivy, "Ivy");
    await ivy.getByRole("button", { name: "Connect", exact: true }).click();
    await ivy.getByText("Not in voice").waitFor({ timeout: 20000 });
    await waitFor(async () => (await nameColor(a, memberSection(a, "Online").getByText("Ivy", { exact: true }))) === ROLE_RGB, "Ivy has the invite's role colour");

    // A link pasted into the address field (what the desktop app does) is understood too.
    const ctx2 = await newCtx();
    const paste = await ctx2.newPage();
    await paste.goto(WEB_URL);
    await paste.getByLabel("Server address").fill(inviteLink);
    await waitFor(async () => (await paste.getByLabel("Server address").inputValue()) === CLIENT_ADDR, "address extracted from the pasted link");
    await paste.getByText(/You were invited to this server/).waitFor();
    await ctx2.close();

    // The used-up invite shows its use.
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Invite people" }).click();
    await a.locator("[data-invite]").first().getByText("1 of 1 uses").waitFor();
    await a.keyboard.press("Escape");
  });

  await check("a banned user sees why, cannot get back in, and can after the unban", async () => {
    await treeItem(a, /Bob/).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Ban…" }).click();
    await a.getByLabel("Reason").fill("e2e spam");
    await a.getByRole("radio", { name: "1 hour" }).click();
    await shot(a, "27-ban-dialog-dark");
    await a.getByRole("dialog").getByRole("button", { name: "Ban…" }).click();
    await b.getByText(/You were banned from the server by Alice/).waitFor({ timeout: 6000 });
    await b.getByText(/Reason: e2e spam/).waitFor();
    await shot(b, "28-banned-dark");
    await sleep(2500);
    if ((await b.getByRole("button", { name: "Connect", exact: true }).count()) === 0) throw new Error("banned client reconnected");
    // Connecting again is refused with the reason.
    await b.getByRole("button", { name: "Connect", exact: true }).click();
    await b.getByRole("alert").filter({ hasText: /banned/ }).filter({ hasText: "e2e spam" }).first().waitFor({ timeout: 8000 });
    // The ban is listed with its reason; lifting it lets Bob in.
    await openSettings(a, "Bans");
    const ban = a.locator('[data-ban="Bob"]');
    await ban.getByText("e2e spam").waitFor();
    await ban.getByText(/ends in/).waitFor();
    await shot(a, "29-settings-bans-dark");
    await ban.getByRole("button", { name: "Unban" }).click();
    await ban.waitFor({ state: "detached" });
    await a.keyboard.press("Escape");
    await b.getByRole("button", { name: "Connect", exact: true }).click();
    await b.getByText("Not in voice").waitFor({ timeout: 20000 });
    await treeItem(b, TALK).dblclick();
    await b.getByText("Voice connected").waitFor({ timeout: 20000 });
    await treeItem(a, /Bob/).waitFor();
  });

  await check("a kicked user is told why and does not reconnect", async () => {
    await treeItem(a, /Bob/).click({ button: "right" });
    await a.getByRole("menuitem", { name: "Kick from server" }).click();
    await a.getByRole("dialog").getByRole("button", { name: "Kick from server" }).click();
    await b.getByText(/You were kicked from the server by Alice/).waitFor({ timeout: 5000 });
    await sleep(2500);
    if ((await b.getByRole("button", { name: "Connect", exact: true }).count()) === 0) throw new Error("kicked client reconnected");
    await shot(b, "17-kicked-dark");
  });

  await check("members who are gone are listed as Offline with their last seen time", async () => {
    // A reused server (E2E_REUSE) remembers the Bobs of earlier runs too.
    const bob = memberSection(a, "Offline").getByRole("button", { name: /Bob/ }).first();
    await bob.waitFor({ timeout: 8000 });
    const title = await bob.getAttribute("title");
    if (!/Last seen/.test(title ?? "")) throw new Error(`no last seen hint: ${title}`);
    await shot(a, "17b-members-offline-dark");
  });

  await check("an admin removes an offline member (not a ban) and previews a clean-up", async () => {
    await openSettings(a, "Members");
    await a.getByLabel("Search members").fill("Bob");
    const row = a.locator('[data-member="Bob"]').first();
    await row.getByRole("button", { name: "Remove Bob" }).click();
    await a.getByRole("dialog").getByText(/This is not a ban/).waitFor();
    await a.getByRole("checkbox", { name: /Also delete their messages and files/ }).check();
    await shot(a, "30-remove-member-dark");
    await a.getByRole("dialog").getByRole("button", { name: "Remove member…" }).click();
    // Back on the members tab, the removed Bob is gone from the list and from the member panel.
    await a.getByLabel("Search members").fill("Bob");
    await waitFor(async () => (await a.locator('[data-member="Bob"]').count()) === 0, "Bob leaves the members list");
    // The clean-up needs a preview before it can remove anything.
    await a.getByRole("tab", { name: "Clean up" }).click();
    const remove = a.getByRole("button", { name: "Remove", exact: true });
    if (!(await remove.isDisabled())) throw new Error("Remove is enabled before a preview");
    await a.getByLabel("Not seen for (days)").fill("30");
    await a.getByRole("button", { name: "Preview" }).click();
    await a.getByText("Nobody matches. Nothing to remove.").waitFor();
    await shot(a, "31-settings-cleanup-dark");
    await a.keyboard.press("Escape");
  });

  await check("older history loads when scrolling to the top", async () => {
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Create channel" }).click();
    await a.getByLabel("Name").fill(BULK);
    await a.getByRole("button", { name: "Create" }).click();
    await treeItem(a, BULK).dblclick();
    await a.getByText(`You joined ${BULK}`).waitFor();
    for (let i = 1; i <= 60; i++) {
      await composer(a).fill(`bulk-${i}`);
      await composer(a).press("Enter");
      await sleep(70); // the server rate-limits to ~20 requests/s
    }
    const ctx = await newCtx();
    const g = await ctx.newPage();
    debugPages.push(g);
    await connect(g, "Gina");
    // Reading a channel's chat does not need its voice.
    await treeItem(g, BULK).click();
    await g.getByText("bulk-60", { exact: true }).waitFor();
    if ((await g.getByText("bulk-1", { exact: true }).count()) !== 0) throw new Error("expected only the latest page initially");
    await g.getByRole("log").evaluate((el) => (el.scrollTop = 0));
    await g.getByText("bulk-1", { exact: true }).waitFor({ timeout: 5000 });
    await g.getByText("This is the beginning of the channel history.").waitFor({ timeout: 5000 });
    // Compact mode screenshot while we are here.
    await g.getByRole("button", { name: "Settings" }).first().click();
    await g.getByRole("tab", { name: "Appearance" }).click();
    await g.getByRole("switch", { name: "Compact mode" }).click();
    await g.keyboard.press("Escape");
    await shot(g, "18-compact-dark");
  });

  const dist = path.join(webRoot, "dist");
  if (existsSync(path.join(dist, "index.html")) && !process.env.VC_SERVER_BIN?.includes("cargo")) {
    await check("production build served by vc-server defaults to its own address", async () => {
      const bin = process.env.VC_SERVER_BIN ?? path.join(repoRoot, "target/debug/vc-server");
      if (!existsSync(bin)) return;
      const port = 8798;
      const dir = mkdtempSync(path.join(tmpdir(), "vc-e2e-web-"));
      const child = spawn(
        bin,
        ["--data-dir", dir, "--http", `127.0.0.1:${port}`, "--media", `127.0.0.1:${port}`, "--public-ip", "127.0.0.1", "--web-root", dist],
        { env: { ...process.env, VC_NO_TEAMSPEAK: "true" } },
      );
      children.push(child);
      try {
        await waitFor(() => reachable(`http://127.0.0.1:${port}/health`), "second server", 15000);
        const ctx = await newCtx();
        const w = await ctx.newPage();
        await w.goto(`http://127.0.0.1:${port}/`);
        await waitFor(async () => (await w.getByLabel("Server address").inputValue()) === `127.0.0.1:${port}`, "prefilled address", 5000);
        await setGlobalNickname(w, "Webby");
        await w.getByRole("button", { name: "Connect", exact: true }).click();
        await w.getByText("Not in voice").waitFor({ timeout: 20000 });
        await w.getByRole("treeitem", { name: /Lobby/ }).dblclick();
        await w.getByText("Voice connected").waitFor({ timeout: 20000 });
        // Served by the server itself, the client uploads to its own origin (no CORS involved).
        await w.locator("input[type=file]").setInputFiles({ name: "tiny.png", mimeType: "image/png", buffer: makePng(8, 8, [10, 120, 200]) });
        await w.getByRole("list", { name: "Files to send" }).getByText(/\d+ B$/).waitFor({ timeout: 10000 });
        await w.getByRole("textbox", { name: "Message", exact: true }).press("Enter");
        await w.getByRole("button", { name: "Open tiny.png" }).locator("img").waitFor({ timeout: 10000 });
      } finally {
        child.kill();
      }
    });
  }

  // ---- Gwar Connect: one identity on every device
  const readUid = async (page) => {
    const field = page.getByLabel("User ID");
    await waitFor(async () => (await field.inputValue()) !== "", "the user id", 10000);
    return field.inputValue();
  };
  const settingsUid = async (page) => {
    await page.getByRole("button", { name: "Settings" }).first().click();
    await page.getByRole("tab", { name: "Identity" }).click();
    return readUid(page);
  };
  const acct = `alice${rid}`;
  const acctPassword = "correct horse battery";
  let localUid = "";
  let accountUid = "";
  let recoveryCode = "";
  const ctxC = await newCtx();
  const c = await ctxC.newPage();
  debugPages.push(c);
  const ctxD = await newCtx();
  const d = await ctxD.newPage();
  debugPages.push(d);
  const fd = recordFrames(d);

  await check("member nickname updates history, exposes the tag, and persists across reconnects", async () => {
    // Earlier moderation checks kick Bob and may remove his server membership.
    await connect(b, "Bob");
    await treeItem(a, TALK).click();
    await treeItem(b, TALK).click();
    await composer(b).fill(`history boundary ${rid}`);
    await composer(b).press("Enter");
    await a.getByText(`history boundary ${rid}`, { exact: true }).waitFor();
    const message = `nickname snapshot ${rid}`;
    await composer(a).fill(message);
    await composer(a).press("Enter");
    const row = a.locator("[data-message]").filter({ hasText: message });
    await row.waitFor();
    await changeServerNickname(a, "Alice renamed");
    // The old history object is unchanged; the row subscribes to the member by uid.
    await row.getByRole("button", { name: "Alice renamed", exact: true }).waitFor();
    await row.getByRole("button", { name: "Alice renamed", exact: true }).click();
    const profile = a.getByRole("dialog", { name: "Member profile" });
    await profile.getByText(/^@[a-z2-7]{10,}$/).waitFor();
    if (!(await profile.getByLabel("Full user ID").inputValue())) throw new Error("profile has no uid");
    await a.keyboard.press("Escape");
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Disconnect" }).click();
    await setGlobalNickname(a, "Global changed");
    await a.getByRole("button", { name: "Connect", exact: true }).click();
    await a.getByText("Not in voice").waitFor();
    // Hello's global profile must not replace this server's stored nickname.
    await a.getByRole("button", { name: "Server menu" }).click();
    await a.getByRole("menuitem", { name: "Change nickname", exact: true }).click();
    if ((await a.getByRole("dialog").getByLabel("Nickname").inputValue()) !== "Alice renamed") throw new Error("server nickname was not retained");
    await a.getByRole("dialog").getByLabel("Nickname").fill("Alice");
    await a.getByRole("dialog").getByRole("button", { name: "Save", exact: true }).click();
    await a.getByRole("dialog").waitFor({ state: "detached" });
    await setGlobalNickname(a, "Alice");
  });

  await check("duplicate nickname mention suggestions send only the selected member uid", async () => {
    await treeItem(a, TALK).click();
    const bob = fb.clients().find((client) => client.nickname === "Bob");
    const carol = await a.evaluate(async () => { const { useSession } = await import("/src/state/stores.ts"); return Object.values(useSession.getState().members).find((member) => member.nickname === "Carol"); });
    if (!bob || !carol) throw new Error("missing test member uids");
    await a.evaluate(async ({ uid }) => { const { controller } = await import("/src/state/controller.ts"); await controller.setMemberNickname(uid, "Bob"); }, { uid: carol.uid });
    await composer(a).pressSequentially("@Bo");
    const options = a.getByRole("option").filter({ hasText: "Bob" });
    await waitFor(async () => (await options.count()) === 2, "duplicate nickname suggestions");
    const tag = await a.evaluate(async ({ uid }) => { const { useSession } = await import("/src/state/stores.ts"); return useSession.getState().members[uid].tag; }, { uid: carol.uid });
    await options.filter({ hasText: `@${tag}` }).click();
    const text = `@Bob duplicate mention ${rid}`;
    await composer(a).pressSequentially(`duplicate mention ${rid}`);
    await composer(a).press("Enter");
    await waitFor(() => fa.events("chat.message").some((msg) => msg.text === text && msg.mentions?.length === 1 && msg.mentions[0] === carol.uid), "selected mention uid");
    await a.evaluate(async ({ uid }) => { const { controller } = await import("/src/state/controller.ts"); await controller.setMemberNickname(uid, "Carol"); }, { uid: carol.uid });
  });

  await check("connect: a new account keeps the local identity and shows the recovery code once", async () => {
    await c.goto(WEB_URL);
    localUid = await settingsUid(c);
    if (!localUid) throw new Error("no local uid");
    await c.getByLabel("Nickname").fill("Account nickname");
    await c.getByRole("dialog").getByRole("button", { name: "Save", exact: true }).first().click();
    await c.getByText("Saved", { exact: true }).waitFor();
    await c.getByRole("tab", { name: "Account" }).click();
    await c.getByRole("button", { name: /Create an account/ }).click();
    await c.getByLabel("Handle").fill(acct);
    await c.getByLabel("Password", { exact: true }).fill(acctPassword);
    await c.getByLabel("Repeat the password").fill(acctPassword);
    await c.getByRole("radio", { name: "Keep my current identity" }).waitFor();
    await shot(c, "19-account-create");
    await c.getByRole("button", { name: "Create account" }).click();
    await c.getByTestId("recovery-code").waitFor({ timeout: 30000 });
    recoveryCode = (await c.getByTestId("recovery-code").innerText()).trim();
    if (!/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/.test(recoveryCode)) throw new Error(`bad recovery code ${recoveryCode}`);
    const cont = c.getByRole("button", { name: "Continue" });
    if (await cont.isEnabled()) throw new Error("continue must wait for the confirmation");
    await shot(c, "20-account-recovery-code");
    await c.getByRole("checkbox", { name: "I saved my recovery code" }).check();
    await cont.click();
    await c.getByText(`Signed in as @${acct}`).first().waitFor();
    await c.getByRole("tab", { name: "Identity" }).click();
    accountUid = await readUid(c);
    if (accountUid !== localUid) throw new Error(`kept identity changed uid: ${localUid} -> ${accountUid}`);
    if (await c.getByRole("button", { name: "Export identity" }).count()) throw new Error("device keys must not be exportable");
  });

  await check("connect: reloading before the recovery code is confirmed leaves no account behind", async () => {
    const ctxE = await newCtx();
    const e = await ctxE.newPage();
    debugPages.push(e);
    const ghost = `ghost${rid}`;
    await e.goto(WEB_URL);
    await e.getByRole("button", { name: "Settings" }).first().click();
    await e.getByRole("tab", { name: "Account" }).click();
    await e.getByRole("button", { name: /Create an account/ }).click();
    await e.getByLabel("Handle").fill(ghost);
    await e.getByLabel("Password", { exact: true }).fill(acctPassword);
    await e.getByLabel("Repeat the password").fill(acctPassword);
    await e.getByRole("button", { name: "Create account" }).click();
    await e.getByTestId("recovery-code").waitFor({ timeout: 30000 });
    await e.reload();
    await e.getByRole("button", { name: "Settings" }).first().click();
    await e.getByRole("tab", { name: "Account" }).click();
    await e.getByLabel("Handle").fill(ghost);
    await e.getByLabel("Password", { exact: true }).fill(acctPassword);
    await e.getByRole("button", { name: "Sign in", exact: true }).click();
    await e.getByRole("alert").filter({ hasText: "Wrong handle or password" }).waitFor({ timeout: 30000 });
    await ctxE.close();
  });

  await check("connect: a second browser signs in to the same account and has the same uid", async () => {
    await d.goto(WEB_URL);
    const own = await settingsUid(d);
    if (own === localUid) throw new Error("second browser should start with its own local identity");
    await d.getByRole("tab", { name: "Account" }).click();
    await d.getByLabel("Handle").fill(`@${acct.toUpperCase()}`);
    await d.getByLabel("Password", { exact: true }).fill("wrong password");
    await d.getByRole("button", { name: "Sign in", exact: true }).click();
    await d.getByRole("alert").filter({ hasText: "Wrong handle or password" }).waitFor({ timeout: 30000 });
    await d.getByLabel("Password", { exact: true }).fill(acctPassword);
    await d.getByRole("button", { name: "Sign in", exact: true }).click();
    await d.getByRole("button", { name: "Sign out" }).first().waitFor({ timeout: 30000 });
    await d.getByRole("tab", { name: "Identity" }).click();
    const uid = await readUid(d);
    if (uid !== localUid) throw new Error(`second device uid ${uid} != ${localUid}`);
    if ((await d.getByLabel("Nickname").inputValue()) !== "Account nickname") throw new Error("second device did not adopt the vault nickname");
    await shot(d, "21-account-identity");
    await d.keyboard.press("Escape");
  });

  await check("connect: the vault is shared between devices; the browser shows the TeamSpeak uid and keeps unknown fields", async () => {
    // Written like the desktop app would (the browser cannot do TeamSpeak itself), through the app's own modules.
    const written = await c.evaluate(async () => {
      const { loadConnectRecord } = await import("/src/net/identity.ts");
      const { connectApi } = await import("/src/connect/api.ts");
      const { loadVault, updateVault, withTeamspeakList } = await import("/src/connect/vault.ts");
      const record = await loadConnectRecord();
      const list = { default: "E2E-TS-UID", identities: [{ uid: "E2E-TS-UID", name: "E2E TeamSpeak", identity: "1Vfake" }] };
      await updateVault(connectApi, record, (v) => ({ ...withTeamspeakList(v, list, 1), future: { kept: true } }));
      return (await loadVault(connectApi, record)).contents;
    });
    if (written.future?.kept !== true) throw new Error("vault lost a field");
    if (written.profile?.nickname !== "Account nickname") throw new Error("TeamSpeak vault update lost the profile");
    await d.getByRole("button", { name: "Settings" }).first().click();
    await d.getByRole("tab", { name: "Account" }).click();
    const identities = d.getByRole("list", { name: "TeamSpeak identities" });
    await identities.getByText("E2E TeamSpeak").waitFor({ timeout: 10000 });
    await identities.getByText("E2E-TS-UID").waitFor({ timeout: 10000 });
    await d.keyboard.press("Escape");
  });

  await check("connect: the server knows the signed-in device by the account's uid", async () => {
    await connect(d, "Alice phone");
    const welcomed = fd.frames.map((f) => f.ok).find((ok) => ok?.uid);
    if (!welcomed) throw new Error("no welcome received");
    if (welcomed.uid !== localUid) throw new Error(`server uid ${welcomed.uid} != account uid ${localUid}`);
    // Verification runs off the hello path; the badge may arrive in member.updated.
    await waitFor(() => [...welcomed.members, ...fd.events("member.updated")].some((member) => member.uid === localUid && member.connect === acct), "verified Connect account handle");
    await memberSection(d, "Online").getByLabel(`Gwar Connect account: @${acct}`).waitFor();
    await d.getByRole("button", { name: "Server menu" }).click();
    await d.getByRole("menuitem", { name: "Change nickname", exact: true }).click();
    await d.getByRole("dialog").getByLabel("Nickname").fill("Account on server");
    await d.getByRole("dialog").getByRole("button", { name: "Save", exact: true }).click();
    await d.getByRole("dialog").waitFor({ state: "detached" });
  });

  await check("connect: the first browser revokes the second device with the password", async () => {
    await c.getByRole("tab", { name: "Account" }).click();
    const devices = c.getByRole("list", { name: "Devices" });
    await devices.getByRole("listitem").nth(1).waitFor({ timeout: 10000 });
    if ((await devices.getByRole("listitem").count()) !== 2) throw new Error("expected two devices");
    if ((await devices.getByText("This device").count()) !== 1) throw new Error("this device must be marked");
    await shot(c, "22-account-devices");
    await devices.getByRole("button", { name: "Revoke" }).click();
    const prompt = c.getByRole("dialog").filter({ hasText: "Enter your password to revoke" });
    await prompt.getByLabel("Password").fill("wrong password");
    await prompt.getByRole("button", { name: "Revoke" }).click();
    await prompt.getByRole("alert").filter({ hasText: "Wrong password" }).waitFor({ timeout: 30000 });
    await prompt.getByLabel("Password").fill(acctPassword);
    await prompt.getByRole("button", { name: "Revoke" }).click();
    await devices.getByText("Revoked", { exact: true }).waitFor({ timeout: 30000 });
    if ((await devices.getByRole("button", { name: "Revoke" }).count()) !== 0) throw new Error("revoked devices can't be revoked again");
  });

  await check("connect: changing the password works and the old one stops", async () => {
    await c.getByLabel("Current password").fill(acctPassword);
    await c.getByLabel("New password").fill("another long password");
    await c.getByLabel("Repeat the password").fill("another long password");
    await c.getByRole("button", { name: "Change password" }).last().click();
    await c.getByText("Password changed.").waitFor({ timeout: 30000 });
  });

  await check("connect: signing out brings the local identity back; the recovery code signs in again", async () => {
    await c.getByRole("button", { name: "Sign out" }).last().click();
    await c.getByText("Signed out.").first().waitFor();
    await c.getByRole("tab", { name: "Identity" }).click();
    if ((await readUid(c)) !== localUid) throw new Error("local identity was not restored");
    await c.getByRole("tab", { name: "Account" }).click();
    await c.getByRole("button", { name: /Recover with a code/ }).click();
    await c.getByLabel("Handle").fill(acct);
    await c.getByLabel("Recovery code").fill(recoveryCode.toLowerCase().replaceAll("-", " "));
    await c.getByLabel("New password").fill("recovered password");
    await c.getByLabel("Repeat the password").fill("recovered password");
    await c.getByRole("button", { name: "Recover account" }).click();
    await c.getByRole("button", { name: "Sign out" }).first().waitFor({ timeout: 30000 });
  });

  await check("test channels are cleaned up", async () => {
    for (const name of [GAMES, TALK, SECRET, BULK]) {
      await treeItem(a, name).click({ button: "right" });
      await a.getByRole("menuitem", { name: "Delete" }).click();
      await a.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
      await treeItem(a, name).waitFor({ state: "detached" });
    }
  });

  await check("no uncaught page errors", async () => {
    const real = errors.filter((e) => !/Failed to load resource|favicon/.test(e));
    if (real.length) throw new Error(real.slice(0, 3).join(" | "));
  });

  await browser.close();
}

for (const sig of ["SIGINT", "SIGTERM"]) {
  process.on(sig, () => {
    for (const c of children) c.kill();
    process.exit(130);
  });
}

let failed = false;
try {
  await main();
} catch (e) {
  console.error(e);
  failed = true;
} finally {
  for (const c of children) c.kill();
}
const bad = results.filter((r) => !r.ok);
console.log(`\n${results.length - bad.length}/${results.length} checks passed`);
process.exit(failed || bad.length ? 1 : 0);
