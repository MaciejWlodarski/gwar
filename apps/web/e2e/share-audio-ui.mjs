/** Real component screenshots, generated media, and responsive control bounds. No service connection. */
import assert from "node:assert/strict";
import { chromium } from "playwright";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
const base = process.env.E2E_SHARE_AUDIO_UI_URL ?? "http://127.0.0.1:18271/e2e/share-audio.html";
const output = process.env.E2E_SHARE_AUDIO_SCREENSHOTS ?? "/tmp/gwar-share-audio-ui";
mkdirSync(output, { recursive: true });
// Headless `--mute-audio` still opens the speakers; this flag gives the graphs no device at all.
const browser = await chromium.launch({ headless: true, args: ["--disable-audio-output"] });
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
      await page.goto(`${base}?lang=${lang}&zero=1`);
      const zero = page.getByRole("status", { name: lang === "pl" ? "Udostępniasz ekran" : "You are sharing your screen" });
      await zero.waitFor();
      await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
      assert.ok((await zero.boundingBox()).height <= 46);
      await page.screenshot({ path: path.join(output, `fixture-zero-${lang}-${name}.png`), animations: "disabled" });
      await page.goto(`${base}?lang=${lang}&choose=1&silent=1`);
      await page.getByRole("dialog").waitFor();
      await page.screenshot({ path: path.join(output, `fixture-no-sound-${lang}-${name}.png`), animations: "disabled" });
      await page.goto(`${base}?lang=${lang}&slow=1`);
      await page.getByTestId("connection-quality").waitFor();
      const count = await page.getByTestId("screen-viewers").evaluate(e => ({ width: e.clientWidth, needed: e.scrollWidth }));
      assert.ok(count.needed <= count.width, `poor connection must not truncate ${lang} watcher count at ${width}px: ${JSON.stringify(count)}`);
      results.at(-1).poorConnectionCount = count;
      await page.goto(`${base}?lang=${lang}&linked=1&slow=1`);
      await page.getByTestId("screen-ts-link").waitFor();
      await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
      await page.evaluate(() => document.fonts.ready);
      const linked = await page.getByRole("status", { name: lang === "pl" ? "Udostępniasz ekran" : "You are sharing your screen" }).evaluate(e => ({ width: e.clientWidth, needed: e.scrollWidth, height: e.getBoundingClientRect().height }));
      const guests = await page.getByTestId("screen-viewers").evaluate(e => ({ width: e.clientWidth, needed: e.scrollWidth, text: e.textContent }));
      assert.ok(linked.needed <= linked.width && linked.height <= 46, `guest link must fit the compact bar at ${width}px: ${JSON.stringify(linked)}`);
      assert.ok(guests.needed <= guests.width, `guest viewer count must fit: ${JSON.stringify(guests)}`);
      assert.equal(guests.text, lang === "pl" ? "2 osoby oglądają" : "2 watching");
      if (width >= 1024) assert.ok(await page.getByTestId("screen-ts-link").getByRole("button", { name: lang === "pl" ? "Wyłącz" : "Turn off", exact: true }).isVisible());
      await page.screenshot({ path: path.join(output, `fixture-linked-${lang}-${name}.png`), animations: "disabled" });
      results.at(-1).guestLink = { ...linked, guests };
    }
  }
  // The desktop app's computer sound: one question before the share, honest states during it.
  for (const lang of ["pl", "en"]) {
    for (const [name, width, height] of [["desktop", 1440, 900], ["tablet", 768, 1024], ["390", 390, 844], ["360", 360, 800]]) {
      await page.setViewportSize({ width, height });
      // Ready is not painted: a narrow window's first video frame comes a moment later.
      const shot = async label => { await page.evaluate(() => document.fonts.ready); await page.waitForTimeout(500); await page.screenshot({ path: path.join(output, `computer-${label}-${lang}-${name}.png`), animations: "disabled" }); };
      const fits = async label => {
        const dialog = page.getByRole("dialog"); await dialog.waitFor();
        await dialog.evaluate(async e => { await Promise.all(e.getAnimations({ subtree: true }).map(a => a.finished)); });
        const box = await dialog.boundingBox();
        assert.ok(box.x >= 0 && box.x + box.width <= width && box.y >= 0 && box.y + box.height <= height, `${label} dialog fits ${width}x${height}: ${JSON.stringify(box)}`);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth), 0);
      };
      for (const state of ["ok", "permission", "unsupported"]) {
        await page.goto(`${base}?lang=${lang}&choose=1&computer=${state}`);
        await page.getByTestId("computer-sound-choice").waitFor();
        await fits(`choose ${state}`);
        await shot(`choose-${state}`);
      }
      for (const state of ["ok", "resuming", "failed", "stopped", "permission"]) {
        await page.goto(`${base}?lang=${lang}&computer=${state}`);
        await page.getByRole("status", { name: lang === "pl" ? "Udostępniasz ekran" : "You are sharing your screen" }).waitFor();
        await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
        assert.ok((await page.getByRole("status", { name: lang === "pl" ? "Udostępniasz ekran" : "You are sharing your screen" }).boundingBox()).height <= 46, "sharing banner stays one compact row");
        assert.equal(await page.getByTestId("share-audio-controls").count(), 0, `${state}: a share without sound has no volume`);
        await shot(`live-${state}`);
        await page.getByRole("button", { name: lang === "pl" ? "Szczegóły" : "Details", exact: true }).click();
        await fits(`details ${state}`);
        await shot(`details-${state}`);
      }
      // Watching somebody else's share with sound while my own has none: the volume is theirs.
      await page.goto(`${base}?lang=${lang}&computer=failed&watchsound=1`);
      await page.getByTestId("share-audio-controls").waitFor();
      await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
      await shot("watching-sound");
    }
  }
  // Sound coming back during a share: a browser's and the desktop app's ways, at rest and on its way.
  for (const lang of ["pl", "en"]) {
    for (const [name, width, height] of [["desktop", 1440, 900], ["tablet", 768, 1024], ["390", 390, 844], ["360", 360, 800]]) {
      await page.setViewportSize({ width, height });
      const sharing = page.getByRole("status", { name: lang === "pl" ? "Udostępniasz ekran" : "You are sharing your screen" });
      for (const [label, params] of [["browser", "silent=1"], ["computer-failed", "computer=failed"], ["computer-stopped", "computer=stopped"], ["computer-permission", "computer=permission"]]) {
        await page.goto(`${base}?lang=${lang}&addable=1&${params}`);
        await sharing.waitFor();
        await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
        await page.evaluate(() => document.fonts.ready);
        await page.waitForTimeout(300);
        const add = page.getByTestId("share-audio-add");
        assert.ok(await add.isVisible(), `${label}: the bar offers sound at ${width}px`);
        const fit = await sharing.evaluate(e => ({ width: e.clientWidth, needed: e.scrollWidth, height: e.getBoundingClientRect().height }));
        assert.ok(fit.needed <= fit.width && fit.height <= 46, `${label} stays one compact row at ${width}px: ${JSON.stringify(fit)}`);
        assert.ok((await add.boundingBox()).height >= 24, `${label}: the button is a real target`);
        const named = lang === "pl" ? "Włącz dźwięk" : "Turn on sound";
        assert.equal(await add.getAttribute("aria-label"), named);
        await add.focus();
        await page.getByRole("tooltip", { name: named }).waitFor({ timeout: 3_000 });
        await page.screenshot({ path: path.join(output, `sound-back-tooltip-${label}-${lang}-${name}.png`), animations: "disabled" });
        await add.blur();
        await page.screenshot({ path: path.join(output, `sound-back-${label}-${lang}-${name}.png`), animations: "disabled" });
        await page.getByRole("button", { name: lang === "pl" ? "Szczegóły" : "Details", exact: true }).click();
        const dialog = page.getByRole("dialog"); await dialog.waitFor();
        await dialog.evaluate(async e => { await Promise.all(e.getAnimations({ subtree: true }).map(a => a.finished)); });
        const box = await dialog.boundingBox();
        assert.ok(box.x >= 0 && box.x + box.width <= width && box.y >= 0 && box.y + box.height <= height, `${label} details fit ${width}x${height}`);
        assert.equal(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth), 0);
        await page.screenshot({ path: path.join(output, `sound-back-details-${label}-${lang}-${name}.png`), animations: "disabled" });
        await page.goto(`${base}?lang=${lang}&addable=1&adding=1&${params}`);
        await sharing.waitFor();
        await page.waitForFunction(() => document.querySelector("video[data-share]")?.readyState >= 2);
        await page.waitForTimeout(300);
        assert.ok(await page.getByTestId("share-audio-add").isDisabled(), `${label}: one add at a time`);
        assert.ok((await sharing.boundingBox()).height <= 46);
        await page.screenshot({ path: path.join(output, `sound-back-adding-${label}-${lang}-${name}.png`), animations: "disabled" });
      }
    }
  }
  writeFileSync(path.join(output, "fixture-layouts.json"), JSON.stringify(results, null, 2));
  console.log(JSON.stringify({ status: "pass", screenshots: output, layouts: results.length }));
} finally { await browser.close(); }
