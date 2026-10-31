#!/usr/bin/env node
/** Actual production surfaces on an offline fixture. Start Vite first. Never uses OS capture. */
import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

const base = process.env.GWAR_VOICE_PEOPLE_URL ?? "http://127.0.0.1:5173";
assert.ok(["127.0.0.1", "localhost"].includes(new URL(base).hostname), "Only loopback is allowed");
const output = process.env.E2E_SCREENSHOTS ?? "/private/tmp/gwar-voice-people-screenshots";
await mkdir(output, { recursive: true });
const browser = await chromium.launch({ headless: true, args: ["--autoplay-policy=no-user-gesture-required"] });
const shots = [], errors = [];
try {
  for (const [device, width, height] of [["desktop", 1440, 900], ["tablet", 768, 1024], ["390", 390, 844], ["360", 360, 800]]) {
    for (const theme of ["dark", "light"]) {
      const context = await browser.newContext({ viewport: { width, height }, locale: "pl-PL", hasTouch: width <= 768, reducedMotion: "reduce" });
      await context.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
      const page = await context.newPage();
      page.on("pageerror", e => errors.push(e.message));
      const open = async view => { await page.goto(`${base}/e2e/voice-people.html?view=${view}&theme=${theme}`); await page.getByText("Gwar", { exact: true }).first().waitFor(); await page.evaluate(() => document.fonts.ready); };
      const shot = async name => {
        assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), `${name}: horizontal overflow`);
        const dialog = page.getByRole("dialog");
        if (await dialog.count()) {
          const box = await dialog.boundingBox();
          assert.ok(box.x >= 0 && box.y >= 0 && box.x + box.width <= width + 1 && box.y + box.height <= height + 1, `${name}: dialog outside viewport`);
        }
        if (["settings", "recording", "preparing", "playing", "restored-preview", "retried-preview"].includes(name)) {
          const fits = await page.locator("[data-mic-playback] [role=status]").evaluate(el => {
            const range = document.createRange(); range.selectNodeContents(el);
            const rects = [...range.getClientRects()], box = el.getBoundingClientRect();
            return rects.length === 1 && rects.every(rect => rect.left >= box.left - 1 && rect.right <= box.right + 1) && el.scrollWidth <= el.clientWidth;
          });
          assert.ok(fits, `${device}/${theme}/${name}: status wraps or clips`);
        }
        const file = `${device}-${theme}-${name}-pl.png`;
        await page.screenshot({ path: path.join(output, file), animations: "disabled" });
        shots.push(file);
      };
      await open("settings");
      if (width < 800) await page.locator('[data-page="audio"]').click();
      const hear = page.getByRole("button", { name: "Posłuchaj siebie", exact: true });
      await hear.scrollIntoViewIfNeeded();
      await shot("settings");
      await hear.click();
      await page.getByText("Mów teraz…", { exact: true }).waitFor();
      // Assert a live recording meter, rather than photographing the first zero frame.
      await page.waitForFunction(() => Number(document.querySelector('[role=meter]')?.getAttribute("aria-valuenow")) >= 50);
      await page.evaluate(() => window.fixtureSetGain(0));
      await page.waitForFunction(() => Number(document.querySelector('[role=meter]')?.getAttribute("aria-valuenow")) <= 20);
      await page.evaluate(() => window.fixtureSetGain(.03));
      await page.waitForFunction(() => Number(document.querySelector('[role=meter]')?.getAttribute("aria-valuenow")) >= 50);
      await page.getByText("Mów teraz…", { exact: true }).waitFor();
      await shot("recording");
      await page.getByText("Przygotowywanie…", { exact: true }).waitFor();
      assert.ok(await page.evaluate(() => window.fixtureTracks.every(t => t.readyState === "ended")));
      assert.equal(await page.evaluate(() => window.fixtureReplays.length), 0, "Sample played before routing completed");
      await shot("preparing");
      await page.evaluate(() => window.fixtureReleaseRoute());
      await page.getByText("Odsłuch…", { exact: true }).waitFor();
      assert.ok(await page.evaluate(() => window.fixtureTracks.every(t => t.readyState === "ended")), "Capture remains live during replay");
      assert.ok(await page.evaluate(() => window.fixtureReplays.length === 1 && window.fixtureReplays[0].liveInputs === 0 && window.fixtureReplays[0].rms > .005), "No audible sample, or feedback path");
      await shot("playing");
      await hear.waitFor();
      await page.waitForFunction(() => window.fixtureTracks.filter(track => track.readyState === "live").length === 1 && Number(document.querySelector('[role=meter]')?.getAttribute("aria-valuenow")) >= 50);
      assert.ok(await page.evaluate(() => window.fixtureSampleReleased()), "Completed sample retains buffers or callbacks");
      await shot("restored-preview");
      // Focus/visibility cancellation is synchronous and never starts another sample automatically.
      await hear.click();
      assert.ok(await page.evaluate(() => { window.dispatchEvent(new Event("blur")); return window.fixtureTracks.every(t => t.readyState === "ended"); }));
      await page.waitForTimeout(100);
      await open("person");
      await page.getByRole("switch", { name: "Wycisz dla mnie", exact: true }).waitFor();
      await shot("person");
      await page.getByRole("switch", { name: "Wycisz dla mnie", exact: true }).click();
      await shot("person-muted");
      const saved = await page.evaluate(() => JSON.parse(localStorage.getItem("gwar.settings")));
      assert.equal(saved.state.userMuted["stable-anna"], true);
      assert.equal(saved.state.userVolumes["stable-anna"], .45);
      await page.getByRole("button", { name: "Zamknij", exact: true }).click();
      await page.getByRole("button", { name: "Opcje: Anna", exact: true }).click();
      assert.equal(await page.getByRole("switch", { name: "Wycisz dla mnie", exact: true }).getAttribute("aria-checked"), "true");
      await page.getByRole("button", { name: "Zamknij", exact: true }).click();
      await shot("channels-muted");
      const joins = page.getByRole("button", { name: "Dołącz do głosu w Wieczorne granie", exact: true });
      assert.ok(await joins.isVisible(), "Voice join requires hover");
      await page.getByRole("button", { name: "Urządzenia", exact: true }).click();
      await page.getByRole("dialog").waitFor();
      await shot("devices");
      await page.getByRole("combobox").first().click();
      await page.getByRole("option", { name: "Mikrofon zestawu słuchawkowego", exact: true }).click();
      assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem("gwar.settings")).state.audio.inputDeviceId), "fixture-input");
      const outputDevice = page.getByRole("combobox").nth(1);
      if (await outputDevice.isEnabled()) {
        await outputDevice.click();
        await page.getByRole("option", { name: "Słuchawki", exact: true }).click();
        assert.equal(await page.evaluate(() => JSON.parse(localStorage.getItem("gwar.settings")).state.audio.outputDeviceId), "fixture-output");
      }
      await open("chat");
      assert.ok(await page.getByRole("button", { name: "Dołącz do głosu", exact: true }).isVisible());
      await shot("chat-header");
      await open("spacer");
      await page.getByRole("heading", { name: "[cspacer1]Rozmowy", exact: true }).waitFor();
      assert.equal(await page.locator("main header").getByRole("button", { name: "Dołącz do głosu", exact: true }).count(), 0, "Spacer header offers voice joining");
      await shot("spacer-header");
      await open("capture-error");
      if (width < 800) await page.locator('[data-page="audio"]').click();
      await page.getByRole("button", { name: "Posłuchaj siebie", exact: true }).click();
      const shutdown = page.getByRole("alert");
      await shutdown.getByText("Nie udało się wyłączyć mikrofonu na czas. Nagranie nie zostało odtworzone.", { exact: true }).waitFor();
      await shutdown.scrollIntoViewIfNeeded();
      assert.ok(await page.evaluate(() => window.fixtureTracks.every(t => t.readyState === "ended")));
      await shot("capture-error");
      const retry = page.getByRole("button", { name: "Ponów podgląd mikrofonu", exact: true });
      await retry.waitFor();
      assert.ok(await page.evaluate(() => {
        window.dispatchEvent(new Event("blur")); window.dispatchEvent(new Event("focus"));
        return window.fixtureTracks.every(t => t.readyState === "ended");
      }), "Timeout automatically reopened capture on focus");
      await retry.click();
      await page.waitForFunction(() => window.fixtureTracks.filter(track => track.readyState === "live").length === 1 && Number(document.querySelector('[role=meter]')?.getAttribute("aria-valuenow")) >= 50);
      assert.equal(await page.evaluate(() => window.fixtureReplays.length), 0, "Silent preview retry played a sample");
      await shot("retried-preview");
      await context.close();
    }
  }
  assert.deepEqual(errors, []);
  await writeFile(path.join(output, "README.md"), `# Voice UX screenshots\n\n${shots.length} screenshots of production React components in a local offline fixture. Polish, both themes, desktop 1440x900, tablet 768x1024, 390x844 and 360x800. Generated microphone tone; no OS microphone or Gwar/TeamSpeak connection. Device labels and participants are fixture data.\n\nChecked: no page overflow; dialogs fit; local mute retains 45% volume; stable-uid settings; visible join controls; record then playback with zero live input tracks; synchronous blur cancellation; live recording level follows input changes; completed samples release buffers/callbacks and restore the eligible silent preview; idle/record/play status fits on one line; spacer headers have no join action; capture-shutdown failure uses the production localized error UI (injected error, no native hardware).\n\n${shots.map(s => `- ${s}`).join("\n")}\n`);
  console.log(`PASS: ${shots.length} screenshots and offline voice UX assertions. ${output}`);
} finally { await browser.close(); }
