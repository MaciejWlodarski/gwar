// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { accountActions } from "../state/account";
import { useSettings } from "../state/settings";
import { FirstRun } from "./FirstRun";
vi.mock("../state/account", () => ({ accountActions: { changeNickname: vi.fn(async () => {}) } }));
let root: Root, container: HTMLDivElement;
beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.mocked(accountActions.changeNickname).mockReset().mockResolvedValue(undefined);
  useSettings.setState({ language: "en", onboarding: { firstUsedAt: 100, completed: false, nudgeDismissed: false, roleReceived: false } });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  await act(async () => root.render(<FirstRun />));
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
describe("name step", () => {
  it("starts empty and requires a valid name before continuing", async () => {
    const input = container.querySelector("input")!;
    expect(input.value).toBe("");
    expect((container.querySelector('[type="submit"]') as HTMLButtonElement).disabled).toBe(true);
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "Alex");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    expect(accountActions.changeNickname).toHaveBeenCalledWith("Alex");
    expect(useSettings.getState().onboarding?.completed).toBe(true);
  });
  it("allows explicit skip, keeping the fallback name", async () => {
    await act(async () => (Array.from(container.querySelectorAll("button")).find(b => b.textContent === "Skip for now")!).click());
    expect(accountActions.changeNickname).not.toHaveBeenCalled();
    expect(useSettings.getState().onboarding?.completed).toBe(true);
  });
  it("keeps the step when saving fails", async () => {
    vi.mocked(accountActions.changeNickname).mockRejectedValue(new Error("offline"));
    await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
    expect(useSettings.getState().onboarding?.completed).toBe(false);
    expect(container.querySelector('[role="alert"]')).not.toBeNull();
  });
});
