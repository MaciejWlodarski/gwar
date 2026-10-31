#!/usr/bin/env node
/** Real shared UI, isolated browser state, loopback only. Native startup IPC is a display fixture. */
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const shots = process.env.E2E_SCREENSHOTS;
const server = spawn("pnpm", ["exec", "vite", "--host", "127.0.0.1", "--port", "5187", "--strictPort"], { cwd: root, stdio: "ignore" });
let browser;
const forbidden = [];
try {
  for (let i = 0; i < 100; i++) {
    try { if ((await fetch("http://127.0.0.1:5187/")).ok) break; } catch { /* server starting */ }
    await new Promise(r => setTimeout(r, 100));
  }
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext();
  await context.route("**/*", route => {
    const url = new URL(route.request().url());
    if (url.hostname === "127.0.0.1" || url.protocol === "data:" || url.protocol === "blob:") return route.continue();
    forbidden.push(url.origin); return route.abort();
  });
  await context.addInitScript(() => localStorage.setItem("gwar.settings", JSON.stringify({ version: 7, state: { language: "pl", theme: "dark", onboarding: { completed: true, firstUsedAt: Date.now(), nudgeDismissed: true, roleReceived: true }, resumeConnection: null } })));
  const page = await context.newPage();
  await page.goto("http://127.0.0.1:5187/");
  await page.getByRole("heading", { name: "Witaj", exact: true }).waitFor();
  const state = async (kind) => page.evaluate(async kind => {
    const { useUi, useSession, useConnectUi } = await import("/src/state/stores.ts");
    const { useSettings } = await import("/src/state/settings.ts");
    const { useAccount } = await import("/src/state/account.ts");
    useAccount.setState({ loaded: true });
    useUi.getState().closeDialog();
    if (kind !== "invite") useConnectUi.setState({ invite: null });
    if (kind === "channels") {
      delete window.__TAURI_INTERNALS__;
      useUi.setState({ section: "servers", pane: "list", overview: false });
      const channel = (id, name) => ({ id, name, parent: null, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: true, private: false, synced: false });
      useSession.setState({ phase: "online", address: "wss://voice.example/ws", endpoint: "wss://voice.example/ws", kind: "gwar", me: null, clients: {}, channels: { 2: channel(2, "Wieczorne granie"), 3: channel(3, "Pogaduchy") }, viewChannel: 2, activeThread: "channel", permissions: [], server: { name: "Nasza ekipa", welcome: "", version: "0.1.2", default_channel: 2, max_clients: 32, upload_limit: 0, file_access: "members", teamspeak: false, screen_guest_links: false, server_chat: { encrypted: false, history: true } } });
      useSettings.getState().saveBookmark({ id: "fixture", name: "Nasza ekipa", address: "wss://voice.example/ws", kind: "gwar" });
      useSettings.getState().setNotificationMode("gwar|wss://voice.example/ws", null, "none");
      useSettings.getState().setNotificationMode("gwar|wss://voice.example/ws", 2, "mentions");
    } else if (kind === "invite") {
      delete window.__TAURI_INTERNALS__;
      useSession.setState({ phase: "idle", server: null, channels: {}, me: null });
      useUi.setState({ section: "servers" });
      useConnectUi.setState({ invite: { code: "friends", server: "wss://voice.example/custom/ws", serverKey: "A".repeat(43), name: "Nasza ekipa" }, inviteLaunch: true });
    } else if (kind.startsWith("startup")) {
      let legacyPresent = kind === "startup-legacy";
      const status = () => ({ supported: kind !== "startup-linux", openAtLogin: false, startMinimized: false, loginStatus: legacyPresent ? "legacy" : kind === "startup-approval" ? "approvalRequired" : kind === "startup-pending" ? "pending" : "disabled", loginControl: kind === "startup-legacy" || kind === "startup-old-empty" ? "removeOnly" : "full", registrationPresent: legacyPresent });
      window.__TAURI_INTERNALS__ = { invoke: async (command, args) => {
        if (command === "desktop_startup") return status();
        if (command === "desktop_set_startup" && kind === "startup-legacy" && args.openAtLogin === false) { legacyPresent = false; return status(); }
        throw new Error(`Unexpected native fixture command ${command}`);
      } };
      useUi.getState().openSettings("appearance");
    } else {
      delete window.__TAURI_INTERNALS__;
      useUi.setState({ section: "servers", pane: "list" });
      useSession.setState({ phase: "idle", address: "wss://voice.example/ws", kind: "gwar", server: { name: "Nasza ekipa" }, channels: { 2: { id: 2, name: "Wieczorne granie" } } });
      useSettings.getState().setNotificationMode("gwar|wss://voice.example/ws", 2, "mentions");
      useUi.getState().openDialog({ kind: "notifications", channel: 2 });
    }
  }, kind);
  async function capture(name, width, language = "pl") {
    await page.evaluate(() => document.fonts.ready);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > innerWidth);
    assert.equal(overflow, false, `${name} ${width}: no page overflow`);
    if (shots) { mkdirSync(shots, { recursive: true }); await page.screenshot({ path: path.join(shots, `${name}-${language}-${width}.png`), animations: "disabled" }); }
  }
  for (const width of [1440, 768, 390, 360]) {
    await page.setViewportSize({ width, height: width >= 768 ? 900 : 844 });
    await state("channels");
    await page.getByRole("button", { name: "Powiadomienia z kanału Wieczorne granie", exact: true }).waitFor();
    await capture("channel-badges", width);
    await page.getByRole("button", { name: "Powiadomienia z kanału Wieczorne granie", exact: true }).click();
    await page.getByRole("radio", { name: "Tylko wzmianki", exact: true }).waitFor();
    assert.equal(await page.getByRole("radio", { name: "Tylko wzmianki", exact: true }).isChecked(), true);
    await page.getByRole("button", { name: "Zamknij", exact: true }).last().click();
    await page.getByRole("button", { name: "Menu serwera", exact: true }).click();
    await page.getByRole("menuitem", { name: "Ustawienia powiadomień", exact: true }).waitFor();
    await capture("server-notifications-menu", width);
    await page.getByRole("menuitem", { name: "Ustawienia powiadomień", exact: true }).click();
    await page.getByRole("radio", { name: "Brak powiadomień", exact: true }).waitFor();
    assert.equal(await page.getByRole("radio", { name: "Brak powiadomień", exact: true }).isChecked(), true);
    await page.getByRole("radio", { name: "Wszystkie wiadomości", exact: true }).check();
    await page.getByRole("button", { name: "Zamknij", exact: true }).last().click();
    await state("notifications");
    await page.getByRole("dialog").waitFor();
    await page.getByRole("radio", { name: "Brak powiadomień", exact: true }).check();
    assert.equal(await page.getByRole("radio", { name: "Brak powiadomień", exact: true }).isChecked(), true);
    await capture("channel-notifications", width);
    await page.getByRole("button", { name: "Zamknij", exact: true }).last().click();
    await state("startup");
    await page.getByRole("switch", { name: /^Uruchamiaj przy logowaniu/ }).waitFor();
    await capture("desktop-startup", width);
    for (const status of ["approval", "pending"]) {
      await state("invite");
      await page.getByRole("button", { name: "Dołącz: Nasza ekipa", exact: true }).waitFor();
      await state(`startup-${status}`);
      await page.getByTestId("startup-status").waitFor();
      assert.equal(await page.getByRole("switch", { name: /^Uruchamiaj przy logowaniu/ }).isChecked(), false);
      const row = await page.getByTestId("startup-status").evaluate(el => ({ height: el.getBoundingClientRect().height, fits: el.scrollWidth <= el.clientWidth, singleLine: el.firstElementChild.getBoundingClientRect().height < 20 }));
      assert.ok(row.height <= 40 && row.fits && row.singleLine, `${width}: compact startup ${status} row`);
      await capture(`desktop-startup-${status}`, width);
      await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "en" }));
      const english = await page.getByTestId("startup-status").evaluate(el => ({ fits: el.scrollWidth <= el.clientWidth, singleLine: el.firstElementChild.getBoundingClientRect().height < 20 }));
      assert.ok(english.fits && english.singleLine, `${width}: compact English startup ${status} row`);
      await capture(`desktop-startup-${status}`, width, "en");
      await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "pl" }));
    }
    await state("invite");
    await page.getByRole("button", { name: "Dołącz: Nasza ekipa", exact: true }).waitFor();
    await state("startup-legacy");
    const saved = page.getByRole("switch", { name: /^Zapisane uruchamianie przy logowaniu/ });
    await saved.waitFor();
    assert.equal(await saved.isChecked(), true);
    for (const step of ["legacy", "removed"]) {
      if (step === "removed") { await saved.click(); assert.equal(await saved.isChecked(), false); assert.equal(await saved.isDisabled(), true); }
      await capture(`desktop-startup-${step}`, width);
      await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "en" }));
      await capture(`desktop-startup-${step}`, width, "en");
      await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "pl" }));
    }
    await state("invite");
    await page.getByRole("button", { name: "Dołącz: Nasza ekipa", exact: true }).waitFor();
    await state("startup-old-empty");
    await page.getByRole("switch", { name: /^Uruchamiaj zminimalizowany/ }).waitFor();
    assert.equal(await page.getByRole("switch", { name: /^Zapisane uruchamianie/ }).count(), 0);
    assert.equal(await page.getByRole("switch", { name: /^Uruchamiaj przy logowaniu/ }).count(), 0);
    await capture("desktop-startup-old-empty", width);
    await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "en" }));
    await capture("desktop-startup-old-empty", width, "en");
    await page.evaluate(async () => (await import("/src/state/settings.ts")).useSettings.setState({ language: "pl" }));
    await state("invite");
    await page.getByRole("button", { name: "Dołącz: Nasza ekipa", exact: true }).waitFor();
    assert.equal(await page.locator('a[href^="gwar://invite/"]').count(), 1);
    const banner = await page.getByTestId("invite-banner").evaluate(el => ({ height: el.getBoundingClientRect().height, fits: el.scrollWidth <= el.clientWidth }));
    assert.ok(banner.height <= 48 && banner.fits, `${width}: compact invite row`);
    await capture("invite", width);
    await page.getByRole("button", { name: "Szczegóły zaproszenia", exact: true }).click();
    await page.getByRole("dialog").waitFor();
    await capture("invite-details", width);
    await state("startup-linux");
    await page.getByRole("switch", { name: /^Działaj w zasobniku/ }).waitFor();
    assert.equal(await page.getByRole("switch", { name: /^Uruchamiaj przy logowaniu/ }).count(), 0);
    assert.equal(await page.getByRole("switch", { name: /^Uruchamiaj zminimalizowany/ }).count(), 0);
    await capture("desktop-startup-linux", width);
    await page.evaluate(async () => { (await import("/src/state/stores.ts")).useUi.getState().openSettings(); });
    await page.getByText("Dźwięki aplikacji", { exact: true }).waitFor();
    await page.getByText("Głos", { exact: true }).waitFor();
    await capture("settings-pages", width);
  }
  assert.deepEqual(forbidden, [], "no production or other external requests");
  console.log("Desktop notifications UI: four widths, persisted choice, compact invite row, details, desktop startup; no external connections.");
} finally {
  await browser?.close(); server.kill("SIGTERM");
}
