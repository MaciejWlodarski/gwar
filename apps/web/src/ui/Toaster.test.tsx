// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useUi } from "../state/stores";
import { Toaster } from "./Toaster";

let root: Root, container: HTMLDivElement;
beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.useFakeTimers();
  Object.defineProperty(document, "hidden", { configurable: true, value: false });
  useUi.setState({ toasts: [] });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  await act(async () => root.render(<Toaster />));
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.useRealTimers(); vi.unstubAllGlobals(); });
const tick = async (ms: number) => act(async () => { await vi.advanceTimersByTimeAsync(ms); });
const toast = async (sticky = false) => act(async () => useUi.getState().toast("error", "Connection needs attention", { key: "connection", sticky }));

describe("toast reading time", () => {
  it("pauses hidden time, including a toast created while hidden, and resumes only the remaining time", async () => {
    await toast(); await tick(2000);
    await act(async () => { Object.defineProperty(document, "hidden", { value: true }); document.dispatchEvent(new Event("visibilitychange")); });
    await tick(60_000); expect(container.textContent).toContain("Connection needs attention");
    await act(async () => { Object.defineProperty(document, "hidden", { value: false }); document.dispatchEvent(new Event("visibilitychange")); });
    await tick(3999); expect(useUi.getState().toasts).toHaveLength(1);
    await tick(1); expect(useUi.getState().toasts).toHaveLength(0);
    Object.defineProperty(document, "hidden", { value: true });
    await toast(); await tick(60_000); expect(useUi.getState().toasts).toHaveLength(1);
  });
  it("pauses on hover and focus; leaving one does not override the other", async () => {
    await toast(); await tick(1000);
    const node = container.querySelector('[data-testid="toast"]')!;
    await act(async () => node.dispatchEvent(new MouseEvent("mouseenter")));
    await tick(10_000);
    await act(async () => (container.querySelector("button") as HTMLButtonElement).focus());
    await act(async () => node.dispatchEvent(new MouseEvent("mouseleave")));
    await tick(10_000); expect(useUi.getState().toasts).toHaveLength(1);
    await act(async () => (container.querySelector("button") as HTMLButtonElement).blur());
    await tick(4999); expect(useUi.getState().toasts).toHaveLength(1);
    await tick(1); expect(useUi.getState().toasts).toHaveLength(0);
  });
  it("keeps state errors through transient traffic, updates by key, runs actions and resolves explicitly", async () => {
    const run = vi.fn();
    await act(async () => {
      useUi.getState().toast("error", "Try again", { key: "state", sticky: true, action: { label: "Retry", run } });
      for (let i = 0; i < 8; i++) useUi.getState().toast("info", `Info ${i}`);
    });
    await tick(60_000); expect(container.textContent).toContain("Try again");
    await act(async () => (Array.from(container.querySelectorAll("button")).find(b => b.textContent === "Retry")!).click());
    expect(run).toHaveBeenCalledOnce();
    await act(async () => useUi.getState().toast("error", "Still waiting", { key: "state", sticky: true }));
    expect(useUi.getState().toasts).toHaveLength(1);
    await act(async () => useUi.getState().dismissToast("state"));
    expect(useUi.getState().toasts).toHaveLength(0);
  });
  it("dismisses a sticky toast using its close button", async () => {
    await toast(true); await tick(60_000);
    await act(async () => (container.querySelector("button") as HTMLButtonElement).click());
    expect(useUi.getState().toasts).toHaveLength(0);
  });
});
