#!/usr/bin/env node
/** Capture the actual Polish stage with a share paused for the viewer's connection. All network is loopback-only. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const output = process.env.GWAR_PAUSED_SCREENSHOTS ?? path.join(homedir(), "dev/projects/gwar-mockups/screen-retry-backoff");
const port = 5198;
const base = `http://127.0.0.1:${port}`;
const vite = spawn(process.execPath, [path.join(root, "apps/web/node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: path.join(root, "apps/web"), stdio: ["ignore", "pipe", "pipe"] });
let logs = "", browser;
vite.stdout.on("data", b => { logs += b; }); vite.stderr.on("data", b => { logs += b; });
const shots = [];
try {
  await mkdir(output, { recursive: true });
  for (let tries = 0; ; tries++) {
    if (vite.exitCode !== null || tries > 600) throw new Error(logs);
    try { if ((await fetch(`${base}/e2e/screen-paused.html`)).ok) break; } catch { /* local server starting */ }
    await new Promise(r => setTimeout(r, 100));
  }
  browser = await chromium.launch({ headless: true });
  for (const [device, width, height] of [["desktop", 1440, 820], ["tablet", 768, 900], ["mobile", 390, 760]]) {
    const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: 2 });
    await context.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", e => errors.push(e.message));
    for (const state of ["paused", "deferred", "retrying", "stopped"]) {
      await page.goto(`${base}/e2e/screen-paused.html?retry=${state === "deferred" ? "deferred" : "now"}${state === "stopped" ? "&waiting" : ""}`);
      const stage = page.locator('[data-screen-paused="bandwidth"]').first();
      await stage.waitFor();
      assert.match(await stage.innerText(), /Twoje łącze nie nadąża/);
      // Stopped retries do not promise the picture comes back on its own; they ask for the click.
      assert.match(await stage.innerText(), state === "stopped" ? /kliknij »Spróbuj ponownie«/ : /obraz wróci sam/);
      if (device !== "mobile") assert.equal(await page.getByTestId("screen-quality").innerText(), "Wstrzymane – słabe łącze");
      const button = stage.getByRole("button");
      if (state === "deferred" || state === "retrying") {
        await button.click();
        assert.equal(await button.isDisabled(), true);
        assert.match(await button.innerText(), state === "deferred" ? /^Ponowna próba za 1[12] s$/ : /Ponawianie…/);
      } else assert.match(await button.innerText(), /Spróbuj ponownie/);
      assert.equal((await button.getAttribute("class")).includes("bg-accent"), state === "stopped");
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `${state}/${device} overflows`);
      const file = path.join(output, `stage-${state}-pl-${device}.png`);
      await page.screenshot({ path: file });
      shots.push(file);
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  await writeFile(path.join(output, "screenshots.txt"), shots.join("\n") + "\n");
  console.log(`Screenshots: ${output}`);
} finally {
  await browser?.close();
  vite.kill("SIGTERM"); // Only the PID created above.
  await new Promise(resolve => vite.exitCode !== null ? resolve() : vite.once("exit", resolve));
}
