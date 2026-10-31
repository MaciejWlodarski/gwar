/** Local-only sound settings QA and owner screenshots. No voice/Connect server is needed. */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { stopProcesses } from "./processes.mjs";

const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const shots = process.env.E2E_SCREENSHOTS ?? "/private/tmp/gwar-sounds-screenshots";
const port = Number(process.env.E2E_SOUND_PORT ?? 5197);
const url = `http://127.0.0.1:${port}`;
mkdirSync(shots, { recursive: true });
const web = spawn("pnpm", ["exec", "vite", "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: webRoot, detached: true, stdio: ["ignore", "pipe", "pipe"] });
let output = "";
web.stdout.on("data", c => { output += c; }); web.stderr.on("data", c => { output += c; });
let browser;
try {
  const until = Date.now() + 60_000;
  while (!(await fetch(url).then(r => r.ok).catch(() => false))) {
    if (Date.now() > until || web.exitCode !== null) throw new Error(`Vite did not start: ${output}`);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  browser = await chromium.launch();
  const context = await browser.newContext({ locale: "pl-PL", colorScheme: "dark" });
  await context.route("**/*", route => ["127.0.0.1", "localhost", "[::1]"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
  await context.routeWebSocket("**/*", socket => {
    if (["127.0.0.1", "localhost", "[::1]"].includes(new URL(socket.url()).hostname)) socket.connectToServer();
    else socket.close();
  });
  await context.addInitScript(() => {
    // Simulate upgrading the previous release, with preferences worth keeping.
    if (!localStorage.getItem("gwar.settings")) localStorage.setItem("gwar.settings", JSON.stringify({ version: 2, state: { language: "pl", theme: "dark", compact: false, onboarding: { completed: true, firstUsedAt: 1, nudgeDismissed: true, roleReceived: false }, audio: { masterVolume: 0.8 } } }));
    window.__soundStarts = 0; window.__micRequests = 0;
    const original = AudioContext.prototype.createBufferSource;
    AudioContext.prototype.createBufferSource = function (...args) {
      const source = original.apply(this, args), start = source.start.bind(source);
      source.start = (...a) => { window.__soundStarts++; start(...a); };
      return source;
    };
    navigator.mediaDevices.getUserMedia = async () => { window.__micRequests++; throw new Error("Sound settings must not open the microphone"); };
  });
  const page = await context.newPage(), errors = [];
  page.on("pageerror", e => errors.push(e.message));
  await page.goto(url);
  const open = () => page.evaluate(async () => {
    const { useUi } = await import("/src/state/stores.ts");
    useUi.setState({ section: "settings", settingsTab: "sounds", settingsDrill: true });
  });
  await open();
  await page.getByRole("switch", { name: "Dźwięki zdarzeń" }).waitFor();
  assert.equal(await page.getByRole("switch").count(), 15);
  const state = () => page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.getState().sounds);
  assert.equal((await state()).enabled, true);
  const firstPreview = page.getByRole("button", { name: "Odtwórz: Dołączasz do rozmowy", exact: true });
  await firstPreview.click();
  await page.waitForFunction(() => window.__soundStarts === 1);
  await page.getByRole("switch", { name: "Ktoś dołącza do Twojego kanału", exact: true }).click();
  assert.equal((await state()).events.peerJoin, false);
  await page.reload(); await open();
  assert.equal((await state()).events.peerJoin, false);
  await page.getByRole("switch", { name: "Ktoś dołącza do Twojego kanału", exact: true }).click();
  await page.getByRole("switch", { name: "Dźwięki zdarzeń" }).click();
  assert.equal(await firstPreview.isDisabled(), true);
  await page.getByRole("switch", { name: "Dźwięki zdarzeń" }).click();
  const saved = await page.evaluate(() => JSON.parse(localStorage.getItem("gwar.settings")));
  assert.equal(saved.version, 6); assert.equal(saved.state.audio.masterVolume, 0.8);
  assert.equal(await page.evaluate(() => window.__micRequests), 0);
  for (const [name, width, height] of [["desktop", 1440, 1000], ["tablet", 768, 1024], ["mobile", 390, 844]]) {
    await page.setViewportSize({ width, height });
    await open();
    await page.evaluate(async () => { await document.fonts.ready; });
    await page.getByRole("switch", { name: "Dźwięki zdarzeń" }).waitFor();
    const scroller = page.locator("main[aria-label=Ustawienia] > div.overflow-y-auto");
    await scroller.evaluate(el => { el.scrollTop = 0; });
    await page.screenshot({ path: path.join(shots, `sounds-pl-${name}-top.png`) });
    if (process.env.E2E_SOUND_INSPECTOR) {
      // Inspect an exact rendered DOM snapshot, without re-running the app's entry point.
      const html = (await page.content()).replace(/<script\b[^>]*>[\s\S]*?<\/script>/gi, "").replace("<head>", `<head><base href="${url}/">`);
      const snapshot = path.join(shots, `sounds-pl-${name}.html`);
      writeFileSync(snapshot, html);
      // Desktop exceptions are inherited chrome: the narrow rail's version/alpha labels,
      // the full-height settings navigation, and the existing readable 720px form cap.
      const allowances = name === "desktop" ? ["--allow", "edge-text,tiny-text,contrast,unused-canvas,panel-void"] : [];
      const result = spawnSync("node", [process.env.E2E_SOUND_INSPECTOR, snapshot, "--app", "--widths", String(width), "--out", path.join(shots, `inspection-${name}`), ...allowances], { encoding: "utf8" });
      console.log(result.stdout, result.stderr);
      if (result.status !== 0) console.warn(`Inspector reported findings at ${width}px; see its report.`);
    }
    await page.getByRole("switch", { name: "Twoje udostępnianie nagle się kończy", exact: true }).scrollIntoViewIfNeeded();
    await page.screenshot({ path: path.join(shots, `sounds-pl-${name}-bottom.png`) });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false);
  }
  assert.deepEqual(errors, []);
  console.log(`PASS: 14 toggles, live preview, no microphone, master switch, migration, reload persistence, no page errors/overflow. Screenshots: ${shots}`);
} finally {
  await browser?.close();
  await stopProcesses([web]);
}
