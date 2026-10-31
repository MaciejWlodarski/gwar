// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { DesktopStartupSettings } from "./DesktopStartupSettings";

const invoke = vi.hoisted(() => vi.fn());
vi.mock("../platform", () => ({ tauri: async () => ({ invoke }) }));
let host: HTMLDivElement, root: ReturnType<typeof createRoot>;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  host = document.createElement("div"); document.body.appendChild(host); root = createRoot(host);
  useSettings.setState({ language: "en" });
  invoke.mockReset().mockResolvedValue({ supported: true, openAtLogin: false, startMinimized: false });
});
afterEach(async () => { await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals(); });
it("reads actual OS state without enabling login and only updates after successful native save", async () => {
  await act(async () => root.render(<DesktopStartupSettings />));
  expect(invoke).toHaveBeenCalledExactlyOnceWith("desktop_startup");
  const switches = host.querySelectorAll<HTMLButtonElement>('[role="switch"]');
  expect(switches).toHaveLength(2);
  invoke.mockRejectedValueOnce(new Error("denied"));
  await act(async () => switches[0]!.click());
  expect(switches[0]!.getAttribute("aria-checked")).toBe("false");
  expect(host.querySelector('[role="alert"]')).not.toBeNull();
  invoke.mockResolvedValueOnce({ supported: true, openAtLogin: true, startMinimized: false });
  await act(async () => switches[0]!.click());
  expect(switches[0]!.getAttribute("aria-checked")).toBe("true");
  expect(invoke).toHaveBeenLastCalledWith("desktop_set_startup", { openAtLogin: true });
});
it("hides startup controls where the native shell cannot support them", async () => {
  useSettings.getState().setDesktop({ openAtLogin: true, startMinimized: true });
  invoke.mockResolvedValue({ supported: false, openAtLogin: false, startMinimized: false });
  await act(async () => root.render(<DesktopStartupSettings />));
  expect(host.querySelector('[role="switch"]')).toBeNull();
  expect(useSettings.getState().desktop).toMatchObject({ openAtLogin: true, startMinimized: true });
  expect(invoke).toHaveBeenCalledExactlyOnceWith("desktop_startup");
});

it("s2 shows a blocked macOS login item as off and opens settings only after a click", async () => {
  useSettings.getState().setDesktop({ openAtLogin: true, startMinimized: false });
  invoke.mockResolvedValue({ supported: true, openAtLogin: false, startMinimized: false, loginStatus: "approvalRequired" });
  await act(async () => root.render(<DesktopStartupSettings />));
  expect(host.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("false");
  expect(host.querySelector('[role="status"]')?.textContent).toContain("Allow Gwar at login");
  expect(invoke).toHaveBeenCalledExactlyOnceWith("desktop_startup");
  const settings = [...host.querySelectorAll("button")].find(b => b.textContent === "System Settings")!;
  await act(async () => settings.click());
  expect(invoke).toHaveBeenLastCalledWith("desktop_open_login_settings");
});

it("s2 refreshes effective authorization after returning from System Settings", async () => {
  invoke.mockResolvedValueOnce({ supported: true, openAtLogin: false, startMinimized: false, loginStatus: "approvalRequired" });
  await act(async () => root.render(<DesktopStartupSettings />));
  invoke.mockResolvedValueOnce({ supported: true, openAtLogin: true, startMinimized: false, loginStatus: "enabled" });
  await act(async () => window.dispatchEvent(new Event("focus")));
  expect(invoke).toHaveBeenLastCalledWith("desktop_startup");
  expect(invoke).toHaveBeenCalledTimes(2);
  expect(host.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("true");
  expect(host.querySelector('[role="status"]')).toBeNull();
});
it("keeps an unconfirmed macOS registration off and offers an explicit status check", async () => {
  invoke.mockResolvedValue({ supported: true, openAtLogin: false, startMinimized: false, loginStatus: "pending" });
  await act(async () => root.render(<DesktopStartupSettings />));
  expect(host.querySelector('[role="status"]')?.textContent).toContain("Startup not confirmed");
  expect(host.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("false");
  const check = [...host.querySelectorAll("button")].find(b => b.textContent === "Check again")!;
  await act(async () => check.click());
  expect(invoke).toHaveBeenCalledTimes(2);
  expect(invoke).toHaveBeenLastCalledWith("desktop_startup");
});
it("s2 keeps the switch off when enabling still needs macOS approval", async () => {
  await act(async () => root.render(<DesktopStartupSettings />));
  invoke.mockResolvedValueOnce({ supported: true, openAtLogin: false, startMinimized: false, loginStatus: "approvalRequired" });
  await act(async () => host.querySelector<HTMLButtonElement>('[role="switch"]')!.click());
  expect(host.querySelector('[role="switch"]')?.getAttribute("aria-checked")).toBe("false");
  expect(host.querySelector('[role="status"]')?.textContent).toContain("Allow Gwar at login");
});
