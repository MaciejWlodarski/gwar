#!/usr/bin/env node
/** Local-only session restoration checks and Polish approval screenshots.
 * Run Vite in e2e mode and an isolated gwar-server first, then set WEB_URL,
 * SERVER_WS, SERVER_LOG (first-start log for its admin token) and SCREENSHOTS.
 * Never uses an installed desktop app or its storage.
 */
import assert from "node:assert/strict";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { chromium } from "playwright";

const web = process.env.WEB_URL ?? "http://127.0.0.1:18973";
const server = process.env.SERVER_WS ?? "ws://127.0.0.1:18979/ws";
for (const address of [web, server]) assert(["127.0.0.1", "localhost", "[::1]"].includes(new URL(address).hostname), "loopback only");
const shots = process.env.SCREENSHOTS ?? "/private/tmp/gwar-session-resilience";
mkdirSync(shots, { recursive: true });
const files = [];
const browser = await chromium.launch({ args: ["--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream"] });
const context = await browser.newContext({ locale: "pl-PL", permissions: ["microphone"], viewport: { width: 1440, height: 900 } });
// Even an accidental default in the app cannot contact a public service in this harness.
await context.route("**/*", route => {
  const url = new URL(route.request().url());
  return ["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) || ["blob:", "data:"].includes(url.protocol) ? route.continue() : route.abort();
});
let websocketMode = "pass";
await context.routeWebSocket(server, socket => {
  if (websocketMode === "fail") socket.close();
  else if (websocketMode === "pass") socket.connectToServer();
});
let page = await context.newPage();
page.on("pageerror", error => console.error("Browser error:", error.message));
page.on("console", message => { if (message.type() === "error") console.error("Browser console:", message.text()); });
const modules = { controller: "/src/state/controller.ts", stores: "/src/state/stores.ts", settings: "/src/state/settings.ts" };
const widths = [["desktop", 1440, 900], ["tablet", 834, 1112], ["mobile", 390, 844], ["mobile-360", 360, 800]];
const snap = async (name, target = page) => {
  await target.evaluate(() => document.fonts.ready);
  const file = path.join(shots, `${name}-pl.png`);
  await target.screenshot({ path: file, animations: "disabled" }); files.push(file);
};
try {
  await page.addInitScript(() => {
    if (!localStorage.getItem("gwar.settings")) localStorage.setItem("gwar.settings", JSON.stringify({ version: 4, state: {
      language: "pl", theme: "dark", audio: { noiseSuppression: false },
      onboarding: { completed: true, firstUsedAt: Date.now(), nudgeDismissed: true, roleReceived: false },
    } }));
  });
  await page.goto(web);
  await page.waitForFunction(() => window.__gwarE2eImport);
  const ok = await page.evaluate(async ({ modules, server }) => {
    const { controller } = await window.__gwarE2eImport(modules.controller);
    return controller.connectInteractive({ address: server }, { remember: true });
  }, { modules, server });
  assert(ok, "local server connected");
  const token = process.env.LOCAL_TOKEN_FILE ? readFileSync(process.env.LOCAL_TOKEN_FILE, "utf8").trim().split("\n").at(-1) :
    /redeem this one-time admin token in your client: (\S+)/.exec(readFileSync(process.env.SERVER_LOG ?? "/private/tmp/gwar-session-server.log", "utf8"))?.[1];
  assert(token, "isolated server's first-start token is required");
  const channel = await page.evaluate(async ({ modules, token }) => {
    const { controller } = await window.__gwarE2eImport(modules.controller);
    await controller.redeemToken(token);
    await controller.updateServer({ name: "Nasza ekipa" });
    const { useSession } = await window.__gwarE2eImport(modules.stores);
    return Object.values(useSession.getState().channels).find(c => c.name === "Pogaduchy")?.id ??
      (await controller.createChannel({ name: "Pogaduchy", encrypted: false })).id;
  }, { modules, token });
  await page.evaluate(async ({ modules, channel }) => {
    const { controller } = await window.__gwarE2eImport(modules.controller);
    const { useSettings } = await window.__gwarE2eImport(modules.settings);
    useSettings.getState().saveBookmark({ id: "other", kind: "gwar", address: "ws://127.0.0.1:18978/ws", name: "Wieczorne granie" });
    controller.setMuted(true); controller.setDeafened(true);
    await controller.joinChannel(channel);
  }, { modules, channel });
  await page.waitForFunction(async modules => {
    const { useVoice } = await window.__gwarE2eImport(modules.stores);
    return useVoice.getState().state === "connected";
  }, modules, { timeout: 30_000 });

  for (const [size, width, height] of widths) {
    await page.setViewportSize({ width, height });
    await page.evaluate(async modules => { const { useUi } = await window.__gwarE2eImport(modules.stores); useUi.getState().landOnServer(); }, modules);
    await page.getByRole("button", { name: "Wieczorne granie", exact: true }).click();
    await page.getByRole("dialog").waitFor();
    assert.match(await page.getByRole("dialog").innerText(), /Opuścić rozmowę na serwerze Nasza ekipa/);
    await snap(`confirm-${size}`);
    await page.getByRole("button", { name: "Anuluj", exact: true }).click();
  }

  // A second tab offers one deliberate takeover action while this tab is in a real call.
  const caller = page;
  const newcomer = await context.newPage();
  await newcomer.goto(web);
  await newcomer.getByRole("button", { name: "Użyj tutaj", exact: true }).waitFor();
  assert.equal(await newcomer.locator("form").count(), 0, "the takeover state replaces the form");
  assert.equal(await newcomer.getByRole("button", { name: "Połącz", exact: true }).count(), 0, "no duplicate connect action");
  assert.equal(await caller.evaluate(async modules => (await window.__gwarE2eImport(modules.stores)).useVoice.getState().state, modules), "connected");
  for (const [size, width, height] of widths) {
    await newcomer.setViewportSize({ width, height });
    await snap(`other-tab-${size}`, newcomer);
  }
  await newcomer.getByRole("button", { name: "Użyj tutaj", exact: true }).click();
  await newcomer.waitForFunction(async modules => {
    const { useSession } = await window.__gwarE2eImport(modules.stores);
    return useSession.getState().phase === "online" && useSession.getState().me !== null;
  }, modules);
  await caller.waitForFunction(async modules => {
    const { useSession } = await window.__gwarE2eImport(modules.stores);
    return useSession.getState().phase === "idle" && useSession.getState().closeReason?.reason?.kind === "replaced";
  }, modules);
  await caller.close(); page = newcomer;

  // Reload restores the server, persisted controls, and no voice participation.
  // A previous build had session preferences but no onboarding preference yet.
  await page.evaluate(() => {
    const saved = JSON.parse(localStorage.getItem("gwar.settings"));
    delete saved.state.onboarding; saved.version = 3;
    localStorage.setItem("gwar.settings", JSON.stringify(saved));
  });
  await page.reload({ waitUntil: "networkidle" });
  console.log("After reload:", await page.evaluate(async modules => {
    const { useSession, useConnectUi } = await window.__gwarE2eImport(modules.stores);
    const { useSettings } = await window.__gwarE2eImport(modules.settings);
    return { phase: useSession.getState().phase, me: !!useSession.getState().me, restoring: useConnectUi.getState().restoring, error: useConnectUi.getState().error, saved: !!useSettings.getState().resumeConnection };
  }, modules));
  await page.waitForFunction(async modules => {
    const { useSession } = await window.__gwarE2eImport(modules.stores);
    const { useConnectUi } = await window.__gwarE2eImport(modules.stores);
    return useSession.getState().phase === "online" && useSession.getState().me !== null && !useConnectUi.getState().restoring;
  }, modules);
  assert.deepEqual(await page.evaluate(async modules => {
    const { useVoice, useSession } = await window.__gwarE2eImport(modules.stores);
    const s = useSession.getState(); const v = useVoice.getState();
    return { muted: v.muted, deafened: v.deafened, state: v.state, channel: s.me ? s.clients[s.me.session].channel : "missing session" };
  }, modules), { muted: true, deafened: true, state: "idle", channel: null });
  await page.evaluate(async modules => (await window.__gwarE2eImport(modules.settings)).useSettings.getState().setOnboarding({ nudgeDismissed: true }), modules);

  // A real failed WebSocket remains cancellable and the online event skips its backoff.
  websocketMode = "fail";
  await page.reload({ waitUntil: "networkidle" });
  await page.waitForFunction(async modules => (await window.__gwarE2eImport(modules.stores)).useConnectUi.getState().startupRetry, modules);
  for (const [size, width, height] of widths) {
    await page.setViewportSize({ width, height });
    await snap(`offline-retry-${size}`);
  }
  websocketMode = "pass";
  await page.evaluate(() => window.dispatchEvent(new Event("online")));
  await page.waitForFunction(async modules => {
    const { useSession, useConnectUi } = await window.__gwarE2eImport(modules.stores);
    return useSession.getState().phase === "online" && useSession.getState().me !== null && !useConnectUi.getState().restoring;
  }, modules);

  // Hold the local WebSocket handshake to capture the actual startup Connecting UI.
  websocketMode = "hold";
  await page.reload();
  await page.getByRole("heading", { name: "Łączenie…", exact: true }).waitFor();
  for (const [size, width, height] of widths) {
    await page.setViewportSize({ width, height });
    await snap(`connecting-${size}`);
  }
  await page.getByRole("button", { name: "Anuluj", exact: true }).click();
  await page.reload();
  await page.waitForFunction(async modules => {
    const { useConnectUi, useSession } = await window.__gwarE2eImport(modules.stores);
    return useSession.getState().phase === "idle" && !useConnectUi.getState().restoring;
  }, modules);
  assert.equal(await page.evaluate(async modules => (await window.__gwarE2eImport(modules.settings)).useSettings.getState().resumeConnection, modules), null);

  // Exercise the real permanent-refusal UI with a controlled capability/snapshot fixture.
  websocketMode = "pass";
  await page.evaluate(async ({ modules, server, channel }) => {
    const { controller } = await window.__gwarE2eImport(modules.controller);
    assertConnected(await controller.connectInteractive({ address: server }, { remember: false }));
    await controller.joinChannel(channel);
    function assertConnected(ok) { if (!ok) throw new Error("Local reconnect failed"); }
  }, { modules, server, channel });
  await page.waitForFunction(async modules => (await window.__gwarE2eImport(modules.stores)).useVoice.getState().state === "connected", modules);
  await page.evaluate(async modules => {
    const { controller } = await window.__gwarE2eImport(modules.controller);
    const { useSession } = await window.__gwarE2eImport(modules.stores);
    const s = useSession.getState(), channel = s.clients[s.me.session].channel;
    controller.voice.capabilities.encryptedVoice = async () => false;
    useSession.setState({ channels: { ...s.channels, [channel]: { ...s.channels[channel], encrypted: true } } });
    controller.retryVoice();
  }, modules);
  await page.waitForFunction(async modules => !!(await window.__gwarE2eImport(modules.stores)).useVoice.getState().failure, modules);
  assert.equal(await page.evaluate(async modules => (await window.__gwarE2eImport(modules.stores)).useUi.getState().toasts.length, modules), 0, "permanent refusal has no toast");
  for (const [size, width, height] of widths) {
    await page.setViewportSize({ width, height });
    await page.evaluate(async modules => (await window.__gwarE2eImport(modules.stores)).useUi.getState().landOnServer(), modules);
    const status = page.getByText("Głos niedostępny", { exact: true });
    assert.equal(await status.count(), 1, "one compact unavailable status");
    assert(await status.evaluate(el => el.scrollWidth <= el.clientWidth), "the complete status fits");
    assert(await page.getByRole("button", { name: "Dlaczego?", exact: true }).evaluate(button => {
      const row = button.parentElement.parentElement, panel = row.parentElement;
      const name = panel.querySelector("span.truncate.text-sm.font-semibold");
      const retry = [...row.querySelectorAll("button")].find(el => el.textContent === "Ponów");
      return name && name.scrollWidth <= name.clientWidth && retry && retry.getBoundingClientRect().top >= row.getBoundingClientRect().top;
    }), "the full name fits and Retry belongs to the status row");
    await snap(`voice-unavailable-${size}`);
    await page.getByRole("button", { name: "Dlaczego?", exact: true }).click();
    await page.getByRole("dialog").waitFor();
    await snap(`voice-unavailable-reason-${size}`);
    await page.getByRole("button", { name: "Zamknij", exact: true }).click();
  }
  writeFileSync(path.join(shots, "screenshots.txt"), `${files.join("\n")}\n`);
  console.log("Local reload, multi-tab protection/takeover, offline recovery, privacy settings, voice exclusion, switch cancellation and explicit-disconnect checks passed.");
  console.log(`${files.length} Polish screenshots: ${shots}`);
} finally {
  await context.close(); await browser.close();
}
