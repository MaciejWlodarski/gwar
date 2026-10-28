#!/usr/bin/env node
/** Capture actual Polish UI with mocked WebRTC stats. All network is loopback-only. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const output = process.env.GWAR_UPLINK_SCREENSHOTS ?? path.join(homedir(), "dev/projects/gwar-mockups/uplink-congestion");
const port = 5197;
const base = `http://127.0.0.1:${port}`;
const vite = spawn(process.execPath, [path.join(root, "apps/web/node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", String(port), "--strictPort"], { cwd: path.join(root, "apps/web"), stdio: ["ignore", "pipe", "pipe"] });
let logs = "", browser;
vite.stdout.on("data", b => { logs += b; }); vite.stderr.on("data", b => { logs += b; });
try {
  await mkdir(output, { recursive: true });
  for (let tries = 0; ; tries++) {
    if (vite.exitCode !== null || tries > 100) throw new Error(logs);
    try { if ((await fetch(`${base}/e2e/uplink.html`)).ok) break; } catch { /* local server starting */ }
    await new Promise(r => setTimeout(r, 100));
  }
  browser = await chromium.launch({ headless: true });
  for (const [device, width] of [["desktop", 1440], ["tablet", 768], ["mobile", 390]]) {
    const context = await browser.newContext({ viewport: { width, height: 900 }, deviceScaleFactor: 1 });
    await context.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
    const page = await context.newPage();
    const errors = [];
    page.on("pageerror", e => errors.push(e.message));
    for (const view of ["screen", "voice", "pill"]) for (const state of ["normal", "congested"]) {
      await page.goto(`${base}/e2e/uplink.html?view=${view}&state=${state}`);
      const target = view === "screen" ? page.getByRole("status", { name: "Udostępniasz ekran" }) : view === "voice" ? page.locator("main > div").first() : page.getByRole("region", { name: "Połączenie głosowe" });
      await target.waitFor();
      if (view === "screen") await page.getByTestId("screen-sender-stats").waitFor();
      const warning = target.getByTestId("upload-warning");
      assert.equal(await warning.count(), state === "congested" ? 1 : 0);
      if (state === "congested") assert.match(await warning.innerText(), /Łącze wysyłania zapchane · 480 ms/);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `${view}/${state}/${device} overflows`);
      await target.screenshot({ path: path.join(output, `${view}-${state}-pl-${device}.png`) });
      if (state === "congested") {
        await warning.locator("summary").click();
        assert.equal(await warning.getAttribute("open"), "");
        await target.screenshot({ path: path.join(output, `${view}-${state}-help-pl-${device}.png`) });
      }
    }
    assert.deepEqual(errors, []);
    await context.close();
  }
  console.log(`Screenshots: ${output}`);
} finally {
  await browser?.close();
  vite.kill("SIGTERM"); // Only the PID created above.
  await new Promise(resolve => vite.exitCode !== null ? resolve() : vite.once("exit", resolve));
}
