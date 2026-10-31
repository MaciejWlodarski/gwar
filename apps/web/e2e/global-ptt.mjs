#!/usr/bin/env node
/** Real rendered settings, fixture OS permission states, no native app or voice server. */
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { stopProcesses } from "./processes.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const scratch = mkdtempSync(path.join(tmpdir(), "gwar-global-ptt-"));
const output = path.resolve(process.env.E2E_SCREENSHOTS ?? path.join(tmpdir(), "gwar-global-ptt-screenshots"));
const children = [], shots = [];
let browser;
try {
  mkdirSync(output, { recursive: true });
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const { port } = probe.address();
  await new Promise(resolve => probe.close(resolve));
  const url = `http://127.0.0.1:${port}`, dist = path.join(scratch, "dist");
  const build = spawnSync("pnpm", ["exec", "vite", "build", "--mode", "e2e", "--outDir", dist], { cwd: root, env: { ...process.env, VITE_CONNECT_URL: "http://127.0.0.1:1" }, encoding: "utf8" });
  assert.equal(build.status, 0, `${build.stdout}${build.stderr}`);
  const vite = path.join(path.dirname(fileURLToPath(import.meta.resolve("vite/package.json"))), "bin/vite.js");
  children.push(spawn(process.execPath, [vite, "preview", "--outDir", dist, "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: root, stdio: "ignore", detached: true }));
  for (const started = Date.now(); ;) {
    try { if ((await fetch(url)).ok) break; } catch { /* Starting local fixture. */ }
    assert.ok(Date.now() - started < 30_000);
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  browser = await chromium.launch();
  console.log("Built offline fixture and launched Chromium");
  const context = await browser.newContext({ locale: "pl-PL", reducedMotion: "reduce" });
  await context.route("**/*", route => ["localhost", "127.0.0.1"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", error => errors.push(String(error)));
  await page.addInitScript(() => {
    localStorage.setItem("gwar.settings", JSON.stringify({ version: 6, state: { language: "pl", theme: "dark", onboarding: { completed: true, firstUsedAt: Date.now(), nudgeDismissed: true, roleReceived: false }, audio: { inputMode: "ptt", pttKey: "KeyV" } } }));
    navigator.mediaDevices.getUserMedia = async () => { throw new Error("Microphone must not open for PTT settings"); };
  });
  const setup = async () => {
    await page.goto(url);
    await page.waitForFunction(() => typeof window.__gwarE2eImport === "function");
    await page.evaluate(async () => {
      const { useUi } = await window.__gwarE2eImport("/src/state/stores.ts");
      useUi.getState().openSettings("audio");
    });
    await page.getByRole("radio", { name: "Naciśnij i mów", exact: true }).waitFor();
  };
  const fixture = async status => page.evaluate(async status => {
    // Set desktop presentation after startup; the real web engine stays offline.
    window.__TAURI_INTERNALS__ = {};
    const { useGlobalPtt } = await window.__gwarE2eImport("/src/platform/ptt.ts");
    const { useSettings } = await window.__gwarE2eImport("/src/state/settings.ts");
    useGlobalPtt.setState({ status, native: status !== "local_only" && status !== "legacy" });
    useSettings.getState().setAudio({ pttKey: "Mouse4" });
  }, status);
  const shot = async (name, state) => {
    await page.locator("[data-input-mode-options]").scrollIntoViewIfNeeded();
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${name}: page overflow`);
    for (const row of [page.locator("[data-input-mode-options]"), page.locator("[data-ptt-status]")]) {
      assert.ok(await row.evaluate(el => el.scrollWidth <= el.clientWidth + 1), `${name}: control overflow`);
    }
    const status = page.locator("[data-ptt-status] p");
    const lines = await status.evaluate(el => el.getBoundingClientRect().height / parseFloat(getComputedStyle(el).lineHeight));
    assert.ok(lines < 1.5, `${name}/${state}: status must stay one compact row (${lines} lines)`);
    const file = `${name}-${state}-pl.png`;
    await page.screenshot({ path: path.join(output, file), animations: "disabled" });
    shots.push(file);
  };
  for (const [name, width, height] of [["desktop", 1440, 1000], ["tablet", 768, 1024], ["mobile-390", 390, 844], ["mobile-360", 360, 800]]) {
    await page.setViewportSize({ width, height });
    await setup();
    await page.getByRole("button", { name: "V", exact: true }).click();
    await page.getByRole("button", { name: "Naciśnij klawisz lub mysz…", exact: true }).waitFor();
    await page.mouse.click(180, 420, { button: "middle" });
    await page.getByRole("button", { name: "Mysz 3", exact: true }).waitFor();
    await page.getByRole("button", { name: "Wybierz przycisk myszy", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Mysz 4", exact: true }).click();
    await page.getByRole("button", { name: "Mysz 4", exact: true }).click();
    await page.keyboard.press("AltRight");
    await page.getByRole("button", { name: "Prawy Alt", exact: true }).waitFor();
    await page.getByRole("button", { name: "Prawy Alt", exact: true }).click();
    await page.keyboard.press("Escape");
    await page.getByRole("button", { name: "Prawy Alt", exact: true }).waitFor();
    const delayLabel = page.getByText("Opóźnienie po puszczeniu", { exact: true });
    assert.ok(await delayLabel.isVisible());
    assert.ok(await delayLabel.evaluate(el => el.getBoundingClientRect().height < parseFloat(getComputedStyle(el).lineHeight) * 1.5), `${name}: release-delay label wraps`);
    const slider = page.getByRole("slider", { name: "Opóźnienie po puszczeniu klawisza", exact: true });
    assert.equal(await slider.getAttribute("aria-valuenow"), "300");
    await slider.focus(); await page.keyboard.press("Home");
    assert.equal(await slider.getAttribute("aria-valuenow"), "0");
    await page.keyboard.press("ArrowRight");
    assert.equal(await slider.getAttribute("aria-valuenow"), "10");
    await fixture("ready");
    await shot(name, "ready");
    console.log(`${name}: binding and delay passed`);
    const heightBefore = await page.locator("[data-input-mode-options]").evaluate(el => el.getBoundingClientRect().height);
    await fixture("permission");
    assert.equal(await page.getByRole("button", { name: "Zezwól", exact: true }).count(), 1);
    await shot(name, "permission");
    assert.equal(await page.locator("[data-input-mode-options]").evaluate(el => el.getBoundingClientRect().height), heightBefore);
    await page.getByRole("button", { name: "Działanie klawisza do mówienia", exact: true }).click();
    await page.getByRole("dialog").waitFor();
    assert.ok(await page.getByRole("dialog").getByText(/Monitorowanie wejścia/).count());
    await page.screenshot({ path: path.join(output, `${name}-details-pl.png`), animations: "disabled" });
    shots.push(`${name}-details-pl.png`);
    await page.keyboard.press("Escape");
    await fixture("recovering"); await shot(name, "recovering");
    await fixture("local_only"); await shot(name, "local-only");
  }
  assert.deepEqual(errors, []);
  writeFileSync(path.join(output, "README.txt"), `Actual built-app screenshots. Desktop permission/recovery states are fixtures, not hardware verification. No production requests or native app launches.\n${shots.join("\n")}\n`);
  console.log(`PASS: binding capture, lone modifier, cancel, delay, compact status, four viewports. Screenshots: ${output}`);
} catch (error) {
  const page = browser?.contexts()[0]?.pages()[0];
  if (page) {
    await page.screenshot({ path: path.join(output, "failure.png"), animations: "disabled" }).catch(() => {});
    console.error(await page.locator("body").innerText().catch(() => "page unavailable"));
  }
  throw error;
} finally {
  await browser?.close();
  await stopProcesses(children);
  rmSync(scratch, { recursive: true, force: true });
}
