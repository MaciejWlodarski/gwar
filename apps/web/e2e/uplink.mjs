#!/usr/bin/env node
/** Capture actual Polish UI with mocked WebRTC stats. All network is loopback-only. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const output = process.env.GWAR_UPLINK_SCREENSHOTS ?? path.join(homedir(), "dev/projects/gwar-mockups/uplink-congestion-v2");
const port = 5197;
const base = `http://127.0.0.1:${port}`;
/** ui/ConnectionQuality.tsx NOTICE_MS, plus a margin. */
const COLLAPSE_MS = 9_000;
const vite = spawn(process.execPath, [path.join(root, "apps/web/node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: path.join(root, "apps/web"), stdio: ["ignore", "pipe", "pipe"] });
let logs = "", browser;
vite.stdout.on("data", b => { logs += b; }); vite.stderr.on("data", b => { logs += b; });
const shots = [];
try {
  await mkdir(output, { recursive: true });
  for (let tries = 0; ; tries++) {
    if (vite.exitCode !== null || tries > 100) throw new Error(logs);
    try { if ((await fetch(`${base}/e2e/uplink.html`)).ok) break; } catch { /* local server starting */ }
    await new Promise(r => setTimeout(r, 100));
  }
  browser = await chromium.launch({ headless: true });
  for (const [device, width, height] of [["desktop", 1440, 900], ["tablet", 768, 1024], ["mobile", 390, 844], ["mobile-360", 360, 800]]) {
    const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: 2 });
    await context.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
    const errors = [];
    const open = async (query) => {
      const page = await context.newPage();
      page.on("pageerror", e => errors.push(e.message));
      await page.clock.install();
      await page.goto(`${base}/e2e/uplink.html?${query}`);
      await page.locator("main").waitFor();
      await page.evaluate(() => document.fonts.ready);
      return page;
    };
    const shot = async (page, name) => {
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `${name} overflows`);
      const file = path.join(output, `${name}-pl-${device}.png`);
      await page.screenshot({ path: file, animations: "disabled" });
      shots.push(file);
    };
    for (const theme of device === "tablet" ? ["dark"] : ["dark", "light"]) {
      const suffix = theme === "light" ? "-light" : "";
      for (const view of theme === "light" ? ["voice"] : ["voice", "pill", "screen"]) {
        const indicator = (page) => (view === "pill" ? page.getByRole("region", { name: "Połączenie głosowe" }) : view === "screen" ? page.getByRole("status", { name: "Udostępniasz ekran" }) : page.locator("aside")).getByTestId("connection-quality");
        if (theme === "dark") {
          const page = await open(`view=${view}&state=normal`);
          if (view === "screen") await page.getByTestId("screen-sender-stats").waitFor();
          // The pill and the sharing bar add the indicator only once the connection is not good.
          assert.equal(await indicator(page).count(), view === "voice" ? 1 : 0);
          if (view === "voice") assert.equal(await indicator(page).innerText(), "20 ms");
          assert.equal(await page.getByTestId("congestion-notice").count(), 0);
          await shot(page, `${view}-normal`);
          await page.close();
        }
        const page = await open(`view=${view}&state=congested&theme=${theme}`);
        if (view === "screen") await page.getByTestId("screen-sender-stats").waitFor();
        const notice = page.getByTestId("congestion-notice");
        await notice.waitFor();
        assert.match(await notice.innerText(), /Twój internet nie nadąża z wysyłaniem — inni słyszą Cię z opóźnieniem ok\. 0,5 s\./);
        assert.equal(await indicator(page).getAttribute("data-quality"), "poor");
        await shot(page, `${view}-congested-notice${suffix}`);
        await page.clock.fastForward(COLLAPSE_MS);
        await notice.waitFor({ state: "detached" });
        await shot(page, `${view}-congested${suffix}`);
        if (view === "voice") {
          await indicator(page).click();
          const details = page.getByRole("dialog", { name: "Jakość połączenia" });
          await details.waitFor();
          assert.match(await details.innerText(), /Zatrzymaj wysyłanie plików/);
          await shot(page, `details${suffix}`);
          await details.getByText("Szczegóły techniczne").click();
          await details.getByText("SQM lub QoS", { exact: false }).waitFor();
          await shot(page, `details-technical${suffix}`);
          await page.keyboard.press("Escape");
          await details.waitFor({ state: "detached" });
          await indicator(page).evaluate(e => new Promise((resolve, reject) => {
            const until = Date.now() + 2000;
            const check = () => e === document.activeElement ? resolve() : Date.now() > until ? reject(new Error("focus does not return to the indicator")) : requestAnimationFrame(check);
            check();
          }));
        }
        await page.close();
      }
    }
    // A temporary loss has one retry-now action in the panel, no voice claims.
    const combined = await open("view=voice&state=congested");
    await combined.getByTestId("congestion-notice").waitFor();
    await combined.evaluate(async () => {
      const { useUi, useVoice } = await import("/src/state/stores.ts");
      const { tNow } = await import("/src/i18n/index.ts");
      useVoice.setState({ state: "failed", reconnecting: true, failure: null });
      useUi.getState().toast("error", tNow("voice.closed"), { key: "voice-closed", sticky: true });
    });
    const panel = combined.locator("aside");
    assert.equal(await combined.getByTestId("congestion-notice").count(), 0);
    assert.equal(await combined.getByTestId("connection-quality").count(), 0);
    const toast = combined.getByTestId("toast");
    assert.equal(await toast.locator("details").count(), 0);
    assert.equal(await combined.getByRole("button", { name: "Ponów", exact: true }).count(), 1);
    const status = panel.getByTestId("voice-status");
    assert.match(await status.innerText(), /Łączenie ponownie…/);
    assert(await status.getByText("Łączenie ponownie…", { exact: true }).evaluate(node => node.scrollWidth <= node.clientWidth), `${device}: reconnecting status truncates`);
    const recoveryText = await status.getByText("Łączenie ponownie…", { exact: true }).boundingBox();
    const recoveryRetry = await panel.getByRole("button", { name: "Ponów", exact: true }).boundingBox();
    assert(recoveryText && recoveryRetry && Math.abs(recoveryText.y - recoveryRetry.y) < 1 && recoveryText.x + recoveryText.width <= recoveryRetry.x, `${device}: reconnecting row wraps or overlaps`);
    assert.equal(await status.locator(".text-danger").count(), 0);
    assert.equal(await panel.getByRole("button", { name: "Dlaczego?", exact: true }).count(), 0);
    if (width < 768) {
      const notice = await toast.boundingBox();
      const controls = await panel.getByRole("button", { name: /Wycisz mikrofon/ }).boundingBox();
      assert(notice && controls && notice.y + notice.height <= controls.y, `${device}: notice covers call controls`);
    }
    await shot(combined, "voice-reconnecting");
    // Permanent refusals have the red explanation row and no retrying toast.
    await combined.evaluate(async () => {
      const { useVoice, useUi } = await import("/src/state/stores.ts");
      useVoice.setState({ state: "failed", reconnecting: false, failure: "Połączenie głosowe jest niedostępne. Ponów, aby spróbować ponownie." });
      useUi.getState().dismissToast("voice-closed");
    });
    assert.equal(await combined.getByTestId("congestion-notice").count(), 0);
    assert.equal(await combined.getByTestId("connection-quality").count(), 0);
    assert.equal(await combined.getByTestId("toast").count(), 0);
    assert.equal(await combined.getByRole("button", { name: "Ponów", exact: true }).count(), 1);
    const labels = [panel.getByText("Głos niedostępny", { exact: true }), panel.getByRole("button", { name: "Dlaczego?", exact: true }), panel.getByRole("button", { name: "Ponów", exact: true })];
    const boxes = await Promise.all(labels.map(label => label.boundingBox()));
    for (let i = 0; i < boxes.length; i++) {
      assert(boxes[i] && Math.abs(boxes[i].y - boxes[0].y) < 1, `${device}: unavailable status wraps`);
      if (i) assert(boxes[i - 1].x + boxes[i - 1].width <= boxes[i].x, `${device}: unavailable status overlaps`);
    }
    await shot(combined, "voice-unavailable-notices");
    await panel.getByRole("button", { name: "Dlaczego?", exact: true }).click();
    await combined.getByRole("dialog", { name: "Głos niedostępny" }).waitFor();
    await combined.keyboard.press("Escape");
    await combined.close();
    // Screen congestion remains local to the share when voice is down.
    const screenOnly = await open("view=screen&state=congested");
    await screenOnly.getByTestId("screen-sender-stats").waitFor();
    await screenOnly.evaluate(async () => {
      const { useVoice } = await import("/src/state/stores.ts");
      useVoice.setState({ state: "failed", reconnecting: true, failure: null });
    });
    assert.equal(await screenOnly.getByTestId("congestion-notice").count(), 0);
    assert.equal(await screenOnly.locator("aside").getByTestId("connection-quality").count(), 0);
    const shareIndicator = screenOnly.getByRole("status", { name: "Udostępniasz ekran" }).getByTestId("connection-quality");
    assert.equal(await shareIndicator.getAttribute("data-quality"), "poor");
    await shot(screenOnly, "screen-voice-reconnecting");
    await shareIndicator.click();
    const screenDetails = screenOnly.getByRole("dialog", { name: "Jakość połączenia" });
    assert.match(await screenDetails.innerText(), /wysyłaniem udostępnianego ekranu/);
    assert.doesNotMatch(await screenDetails.innerText(), /słyszą Cię/);
    await screenOnly.close();
    // The pill's indicator must preserve truncation for real long names.
    for (const state of ["normal", "congested"]) {
      const page = await open(`view=pill&state=${state}&long=1`);
      await page.getByRole("region", { name: "Połączenie głosowe" }).waitFor();
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `long pill/${state}/${device} overflows`);
      await page.close();
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  await writeFile(path.join(output, "screenshots.txt"), `${shots.join("\n")}\n`);
  console.log(`Screenshots: ${output}`);
} finally {
  await browser?.close();
  vite.kill("SIGTERM"); // Only the PID created above.
  await new Promise(resolve => vite.exitCode !== null ? resolve() : vite.once("exit", resolve));
}
