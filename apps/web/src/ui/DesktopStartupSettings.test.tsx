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
  invoke.mockResolvedValue({ supported: false, openAtLogin: false, startMinimized: false });
  await act(async () => root.render(<DesktopStartupSettings />));
  expect(host.querySelector('[role="switch"]')).toBeNull();
});
