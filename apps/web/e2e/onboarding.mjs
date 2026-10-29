#!/usr/bin/env node
/** Real app + isolated loopback server. Optional Polish owner-review screenshots.
 * GWAR_SERVER_BIN=/path/to/gwar-server E2E_SCREENSHOTS=/scratch pnpm --filter web e2e:onboarding
 * No production services, desktop application data or stored user identities are used.
 */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";


const webRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repo = path.resolve(webRoot, "../..");
const children = [];
const shots = process.env.E2E_SCREENSHOTS;
const captured = [];
let browser;
const delay = (ms) => new Promise(resolve => setTimeout(resolve, ms));
async function port() {
  const probe = net.createServer();
  await new Promise(resolve => probe.listen(0, "127.0.0.1", resolve));
  const number = probe.address().port;
  await new Promise(resolve => probe.close(resolve));
  return number;
}
function start(command, args, options = {}) {
  const child = spawn(command, args, { cwd: repo, detached: true, stdio: ["ignore", "pipe", "pipe"], ...options });
  child.output = "";
  child.stdout.on("data", b => { child.output += b; });
  child.stderr.on("data", b => { child.output += b; });
  children.push(child);
  return child;
}
async function until(check, label) {
  const end = Date.now() + 30_000;
  while (Date.now() < end) { if (await check()) return; await delay(100); }
  throw new Error(`Timed out: ${label}\n${children.map(c => c.output).join("\n")}`);
}
async function capture(page, name, width) {
  if (!shots) return;
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: path.join(shots, `${name}-pl-${width}.png`), animations: "disabled" });
  captured.push(`${name}-pl-${width}.png`);
}
async function bannerStyle(page, id) {
  return page.getByTestId(id).evaluate(banner => {
    const text = banner.querySelector("p"), icon = banner.querySelector("svg"), buttons = [...banner.querySelectorAll("button")];
    const bounds = banner.getBoundingClientRect(), last = buttons.at(-1).getBoundingClientRect();
    const centers = [icon, text, ...buttons].map(el => { const b = el.getBoundingClientRect(); return b.y + b.height / 2; });
    return { height: bounds.height, textHeight: text.getBoundingClientRect().height, lineHeight: parseFloat(getComputedStyle(text).lineHeight), iconSize: icon.getBoundingClientRect().width, iconColor: getComputedStyle(icon).color, buttonHeight: buttons[0].getBoundingClientRect().height, fits: banner.scrollWidth <= banner.clientWidth && text.scrollWidth <= text.clientWidth, centerSpread: Math.max(...centers) - Math.min(...centers), rightGap: bounds.right - last.right, paddingRight: parseFloat(getComputedStyle(banner).paddingRight) };
  });
}
function assertOneRow(style, label) {
  assert.equal(style.height, 57, `${label}: single-line banner height`);
  assert.ok(Math.abs(style.textHeight - style.lineHeight) < 1, `${label}: one text line`);
  assert.ok(style.fits, `${label}: text fits without clipping or overflow`);
  assert.ok(style.centerSpread < 1, `${label}: icon, text and controls align`);
  assert.equal(style.rightGap, style.paddingRight, `${label}: last control sits at the right edge`);
}
async function mobileBanners(page, id, name) {
  for (const width of [360, 390]) {
    await page.setViewportSize({ width, height: 844 });
    for (const language of ["en", "pl"]) {
      await state(page, { settings: { language } });
      assertOneRow(await bannerStyle(page, id), `${id} ${language} ${width}px`);
    }
    await capture(page, name, width === 390 ? "mobile" : "mobile-360");
    if (id === "identity-banner") {
      await page.getByRole("button", { name: "Zabezpiecz", exact: true }).click();
      await page.getByRole("dialog").getByRole("button", { name: "Załóż konto Gwar Connect", exact: true }).waitFor();
      await capture(page, "identity-protect", width === 390 ? "mobile" : "mobile-360");
      await page.getByRole("dialog").getByRole("button", { name: "Zamknij", exact: true }).click();
      assert.ok(await page.getByTestId(id).isVisible(), "closing choices keeps the reminder");
    }
  }
}
async function state(page, values) {
  await page.evaluate(async values => {
    const { useSettings } = await import("/src/state/settings.ts");
    const { useAccount } = await import("/src/state/account.ts");
    const { useUi } = await import("/src/state/stores.ts");
    if (values.settings) useSettings.setState(values.settings);
    if (values.account) useAccount.setState(values.account);
    if (values.ui) useUi.setState(values.ui);
  }, values);
}

try {
  if (shots) mkdirSync(shots, { recursive: true });
  const webPort = await port(), serverPort = await port(), connectPort = await port();
  const base = `http://127.0.0.1:${webPort}`, endpoint = `127.0.0.1:${serverPort}`;
  const connectBase = `http://127.0.0.1:${connectPort}`;
  start(process.env.GWAR_CONNECT_BIN ?? path.join(repo, "target/debug/gwar-connect"), ["--data-dir", mkdtempSync(path.join(tmpdir(), "gwar-onboarding-connect-")), "--http", `127.0.0.1:${connectPort}`, "--origin", base]);
  const server = start(process.env.GWAR_SERVER_BIN ?? path.join(repo, "target/debug/gwar-server"), ["--data-dir", mkdtempSync(path.join(tmpdir(), "gwar-onboarding-server-")), "--http", endpoint, "--media", endpoint, "--public-ip", "127.0.0.1", "--public-endpoint", `ws://${endpoint}/ws`, "--web-origin", base, "--connect-url", connectBase]);
  start(process.execPath, [path.join(webRoot, "node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", String(webPort), "--strictPort"], { cwd: webRoot, env: { ...process.env, VITE_CONNECT_URL: connectBase } });
  await until(async () => { try { return (await fetch(base)).ok && /server ready/.test(server.output); } catch { return false; } }, "local app and server");
  const adminToken = /redeem this one-time admin token in your client: (\S+)/.exec(server.output)?.[1];
  assert.ok(adminToken);
  browser = await chromium.launch({ headless: true, args: ["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"] });
  for (const [width, viewport] of [["desktop", { width: 1440, height: 900 }], ["mobile", { width: 390, height: 844 }]]) {
    const context = await browser.newContext({ viewport, locale: "pl-PL", colorScheme: "dark", acceptDownloads: true });
    // Every request is restricted to this harness's loopback services.
    await context.route("**/*", route => {
      const url = new URL(route.request().url());
      return ["127.0.0.1", "localhost"].includes(url.hostname) ? route.continue() : route.abort();
    });
    const page = await context.newPage();
    await page.goto(base);
    await page.getByRole("heading", { name: "Jak mają Cię widzieć inni?" }).waitFor();
    assert.equal(await page.getByLabel("Twoja nazwa").inputValue(), "");
    assert.equal(await page.getByRole("button", { name: "Dodaj serwer", exact: true }).count(), 0);
    const gated = await page.evaluate(async endpoint => {
      const { controller } = await import("/src/state/controller.ts");
      return controller.connectInteractive({ address: `ws://${endpoint}/ws` }, { remember: false });
    }, endpoint);
    assert.equal(gated, false, "other connect entry points must show the name step first");
    await capture(page, "first-run", width);
    await page.reload();
    await page.getByRole("heading", { name: "Jak mają Cię widzieć inni?" }).waitFor();
    await page.getByLabel("Twoja nazwa").fill("Ola");
    await page.getByRole("button", { name: "Dalej", exact: true }).click();
    await page.getByLabel("Adres serwera").waitFor();
    await page.getByRole("button", { name: "Dodaj serwer", exact: true }).waitFor();
    assert.equal(await page.getByRole("radiogroup").isVisible(), false);
    await capture(page, "connect-collapsed", width);
    await page.getByText("Zaawansowane", { exact: true }).click();
    await page.getByRole("radio", { name: "TeamSpeak" }).waitFor();
    await capture(page, "connect-advanced", width);
    await page.getByText("Zaawansowane", { exact: true }).click();
    // Test the public option without ever opening a production WebSocket.
    await page.evaluate(async () => {
      const { controller } = await import("/src/state/controller.ts");
      const original = controller.connectInteractive.bind(controller);
      controller.connectInteractive = async (params, opts) => {
        if (params.address === "gwar.app") { window.__publicParams = params; return false; }
        return original(params, opts);
      };
    });
    await page.getByRole("button", { name: "Dołącz do publicznego serwera (gwar.app)", exact: true }).click();
    assert.deepEqual(await page.evaluate(() => ({ kind: window.__publicParams.kind, address: window.__publicParams.address, insecure: window.__publicParams.insecureLan })), { kind: "gwar", address: "gwar.app", insecure: false });
    // The real first connect must send the chosen name and preserve it across reload.
    await page.getByLabel("Adres serwera").fill(`ws://${endpoint}/ws`);
    await page.getByRole("button", { name: "Połącz", exact: true }).click();
    await page.getByRole("button", { name: "Menu serwera" }).waitFor();
    assert.equal(await page.evaluate(async () => (await (await import("/src/state/controller.ts")).controller.getIdentity()).nickname), "Ola");
    // Seed a received role through the real reducer/store, as server events do.
    await page.evaluate(async () => {
      const { useSession } = await import("/src/state/stores.ts");
      const s = useSession.getState(), me = s.members[s.me.uid];
      useSession.setState({ members: { ...s.members, [me.uid]: { ...me, groups: [1, 2] } } });
    });
    await page.getByTestId("identity-banner").waitFor();
    // The reminder must fit one desktop line at 1280 px, like the expiry notice.
    if (width === "desktop") await page.setViewportSize({ width: 1280, height: 900 });
    await page.evaluate(() => document.fonts.ready);
    const reminderStyle = await bannerStyle(page, "identity-banner");
    assertOneRow(reminderStyle, `identity reminder ${width}`);
    assert.equal(await page.getByTestId("identity-banner").getByText(/klucz prywatny/).count(), 0);
    assert.ok(await page.getByTestId("identity-banner").evaluate(banner => banner.scrollWidth <= banner.clientWidth), "compact reminder fits the viewport");
    if (width === "mobile") await mobileBanners(page, "identity-banner", "identity-banner");
    else {
      await capture(page, "identity-banner", width);
      // Open the mobile choices, then resize: the same Dialog must work on desktop too.
      await page.setViewportSize({ width: 390, height: 844 });
      await page.getByRole("button", { name: "Zabezpiecz", exact: true }).click();
      await page.setViewportSize({ width: 1280, height: 900 });
      await page.getByRole("dialog").getByRole("button", { name: "Załóż konto Gwar Connect", exact: true }).waitFor();
      await capture(page, "identity-protect", width);
      await page.getByRole("dialog").getByRole("button", { name: "Zamknij", exact: true }).click();
    }
    if (width === "mobile") await page.getByRole("button", { name: "Zabezpiecz", exact: true }).click();
    await page.getByRole("button", { name: "Zapisz kopię", exact: true }).click();
    await page.getByRole("dialog").getByText(/Plik zawiera Twój klucz prywatny/).waitFor();
    await capture(page, "identity-backup-confirm", width);
    await page.getByRole("button", { name: "Anuluj", exact: true }).click();
    assert.ok(await page.getByTestId("identity-banner").isVisible(), "cancelled confirmation keeps the reminder");
    if (width === "mobile") await page.getByRole("button", { name: "Zabezpiecz", exact: true }).click();
    await page.getByRole("button", { name: "Zapisz kopię", exact: true }).click();
    const download = page.waitForEvent("download");
    await page.getByRole("dialog").getByRole("button", { name: "Zapisz", exact: true }).click();
    assert.match((await download).suggestedFilename(), /gwar-identity-backup/);
    await page.getByTestId("identity-banner").waitFor({ state: "detached" });
    // Session-expiry states are seeded; no real user account or certificate is used.
    const fakeAccount = { handle: "ola", accountKey: "review-account", deviceKey: "review-device", deviceName: "Browser", expiresAt: Date.now() + 5 * 86_400_000 };
    await state(page, { account: { account: fakeAccount, certChecked: true } });
    await page.getByTestId("expiry-banner").getByText(width === "mobile" ? /Zaloguj się w ciągu 5 dni/ : /Za 5 dni/).waitFor();
    const expiryStyle = await bannerStyle(page, "expiry-banner");
    assertOneRow(expiryStyle, `expiry warning ${width}`);
    for (const key of ["iconSize", "iconColor", "buttonHeight"]) assert.equal(reminderStyle[key], expiryStyle[key], `reminder matches expiry ${key}`);
    if (width === "desktop") await page.setViewportSize(viewport);
    if (width === "mobile") await mobileBanners(page, "expiry-banner", "expiry-before");
    else await capture(page, "expiry-before", width);
    await page.getByRole("button", { name: width === "mobile" ? "Zaloguj" : "Zaloguj ponownie", exact: true }).click();
    await page.getByRole("dialog").getByLabel("Hasło", { exact: true }).waitFor();
    await page.getByRole("button", { name: "Anuluj", exact: true }).click();
    await state(page, { account: { account: { ...fakeAccount, expiresAt: 1 } } });
    await page.getByTestId("expiry-banner").getByText(width === "mobile" ? /Logowanie wygasło/ : /Wpisz ponownie hasło/).waitFor();
    if (width === "mobile") await mobileBanners(page, "expiry-banner", "expiry-after");
    else await capture(page, "expiry-after", width);
    await state(page, { account: { account: null } });
    for (const [name, key, kind] of [["voice-disconnected", "voice.disconnected", "info"], ["voice-reconnecting", "voice.closed", "info"], ["voice-reconnect-failed", "voice.reconnectFailed", "error"]]) {
      await page.evaluate(async ({ key, kind }) => {
        const { useUi } = await import("/src/state/stores.ts");
        const { tNow } = await import("/src/i18n/index.ts");
        const { controller } = await import("/src/state/controller.ts");
        useUi.getState().toast(kind, tNow(key), { key: "voice-closed", sticky: true, action: { label: tNow("voice.retry"), run: () => controller.retryVoice() } });
      }, { key, kind });
      await capture(page, name, width);
    }
    await page.evaluate(async () => (await import("/src/state/stores.ts")).useUi.getState().dismissToast("voice-closed"));
    await page.evaluate(async () => {
      const { useUi } = await import("/src/state/stores.ts");
      useUi.getState().toast("error", "Nie słychać rozmowy. Sprawdź ustawienia dźwięku.", { key: "review-sticky", sticky: true, action: { label: "Ustawienia dźwięku", run: () => useUi.getState().openSettings("audio") } });
    });
    await page.getByTestId("toast").waitFor();
    await page.waitForTimeout(6500);
    assert.ok(await page.getByTestId("toast").isVisible());
    if (width === "mobile") {
      const toast = await page.getByTestId("toast").boundingBox();
      const panel = await page.getByText("Poza kanałem głosowym", { exact: true }).locator("xpath=ancestor::div[contains(@class, 'rounded-xl')][1]").boundingBox();
      assert.ok(toast && panel && toast.y + toast.height <= panel.y, "sticky toast stays above the user panel and tab bar");
    }
    await capture(page, "sticky-toast", width);
    await page.getByRole("button", { name: "Ustawienia dźwięku", exact: true }).click();
    await page.getByRole("main", { name: "Ustawienia" }).waitFor();
    await page.reload();
    if (width === "mobile") await page.getByRole("navigation", { name: "Sekcje" }).getByRole("button", { name: "Serwery", exact: true }).click();
    await page.getByLabel("Adres serwera").waitFor();
    assert.equal(await page.getByTestId("identity-banner").count(), 0);
    assert.equal(await page.getByRole("heading", { name: "Jak mają Cię widzieć inni?" }).count(), 0);
    await context.close();
    console.log(`ok onboarding, public option, local connect, backup, expiry actions, sticky action, reload (${width})`);
  }
  if (shots) writeFileSync(path.join(shots, "screenshots.txt"), [
    "Real running web app, Polish, Chromium, graphite/cyan. Desktop 1440x900; mobile 390x844.",
    "Identity reminder and backup confirmation desktop captures are 1280x900; the reminder matches expiry height, icon treatment and buttons at that width.",
    "Identity and expiry mobile banners are one row at 360 and 390 px in both locales. Polish screenshots include both widths and the protection choices dialog.",
    "The desktop protection dialog is opened on mobile and resized to desktop to verify the same dialog across widths.",
    "Captured with apps/web/e2e/onboarding.mjs. Only throwaway loopback services and browser contexts.",
    "First run and connection are real flows. Role receipt and before/after expiry are seeded UI states.",
    "The sticky toast uses the generic production component with a seeded audio error and working Settings action.",
    "Voice notices use the production localized strings and keyed toast component with seeded disconnected, reconnecting and exhausted states.",
    "The private-key warning appears only in the backup confirmation. Cancelling keeps the reminder; confirming produces a real backup download.",
    "The harness also verifies that a mobile toast stays above the user panel and tab bar.",
    "No production connections or desktop user data. Public-server button intercepted before connection.",
    ...captured,
  ].join("\n") + "\n");
} catch (error) {
  console.error(error);
  process.exitCode = 1;
} finally {
  await browser?.close();
  // Direct executables: no package-manager child or process-group permission is needed.
  for (const child of children) {
    if (child.exitCode !== null || child.signalCode !== null) continue;
    child.kill("SIGTERM");
    await Promise.race([new Promise(resolve => child.once("exit", resolve)), delay(5000)]);
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
      await new Promise(resolve => child.once("exit", resolve));
    }
  }
}
