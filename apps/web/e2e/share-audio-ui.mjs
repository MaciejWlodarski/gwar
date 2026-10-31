/** Real component screenshots, generated media, and responsive control bounds. No service connection. */
import assert from "node:assert/strict";
import { chromium } from "playwright";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
const base = process.env.E2E_SHARE_AUDIO_UI_URL ?? "http://127.0.0.1:18271/e2e/share-audio.html";
const output = process.env.E2E_SHARE_AUDIO_SCREENSHOTS ?? "/tmp/gwar-share-audio-ui";
mkdirSync(output, { recursive: true });
const browser = await chromium.launch({ headless: true });
const results = [];
try {
  const page = await browser.newPage();
  await page.route("**/*", route => {
    const url = new URL(route.request().url());
    return ["127.0.0.1", "localhost"].includes(url.hostname) ? route.continue() : route.abort();
  });
  for (const lang of ["pl", "en"]) {
    for (const [name, width, height] of [["desktop", 1440, 900], ["tablet", 768, 1024], ["390", 390, 844], ["360", 360, 800]]) {
      await page.setViewportSize({ width, height });
      await page.goto(`${base}?lang=${lang}`);
      await page.getByTestId("share-audio-controls").waitFor();
      await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
      const bounds = await page.evaluate(() => {
        const bar = document.querySelector('[role="status"]');
        const rect = bar.getBoundingClientRect();
        const children = [...bar.children].filter(e => getComputedStyle(e).display !== "none").map(e => { const r = e.getBoundingClientRect(); return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, overflow: e.scrollWidth - e.clientWidth }; });
        return { width: innerWidth, overflow: document.documentElement.scrollWidth - innerWidth, height: rect.height, children };
      });
      assert.equal(bounds.width, width); assert.equal(bounds.overflow, 0);
      assert.ok(bounds.height <= 46, "sharing banner remains one compact row");
      assert.ok(bounds.children.every(c => c.left >= 0 && c.right <= width && c.overflow <= 1), JSON.stringify({ lang, name, ...bounds }));
      results.push({ lang, name, ...bounds });
      await page.screenshot({ path: path.join(output, `fixture-live-${lang}-${name}.png`), animations: "disabled" });
      await page.getByRole("button", { name: lang === "pl" ? "Szczegóły" : "Details", exact: true }).click();
      const dialog = page.getByRole("dialog"); await dialog.waitFor();
      await dialog.evaluate(async e => { await Promise.all(e.getAnimations({ subtree: true }).map(a => a.finished)); });
      const box = await dialog.boundingBox(); assert.ok(box.x >= 0 && box.x + box.width <= width && box.y >= 0 && box.y + box.height <= height);
      await page.screenshot({ path: path.join(output, `fixture-details-${lang}-${name}.png`), animations: "disabled" });
      await page.goto(`${base}?lang=${lang}&choose=1&silent=1`);
      await page.getByRole("dialog").waitFor();
      await page.screenshot({ path: path.join(output, `fixture-no-sound-${lang}-${name}.png`), animations: "disabled" });
    }
  }
  writeFileSync(path.join(output, "fixture-layouts.json"), JSON.stringify(results, null, 2));
  console.log(JSON.stringify({ status: "pass", screenshots: output, layouts: results.length }));
} finally { await browser.close(); }
