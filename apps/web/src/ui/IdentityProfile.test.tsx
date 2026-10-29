// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { SettingsSection } from "./Settings";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ update: vi.fn(async (_: object) => ({})), importIdentity: vi.fn(async (_: string) => {}) }));
vi.mock("../state/controller", () => ({
  controller: { updateProfile: mock.update, importIdentity: mock.importIdentity, getIdentity: () => new Promise(() => {}) },
  describeVoiceError: String,
  UploadError: class extends Error {},
}));

describe("profile", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.update.mockClear();
    mock.importIdentity.mockReset().mockResolvedValue(undefined);
    useUi.setState({ toasts: [] });
    useSettings.setState({ language: "en", onboarding: { completed: false, firstUsedAt: 1, nudgeDismissed: false, roleReceived: false } });
    // A TeamSpeak away message cleaned for display (it ended in a bidi control).
    useSession.setState({ phase: "online", me: { session: 1, uid: "ts:me" } as never, clients: { 1: { id: 1, uid: "ts:me", away: "Busy" } as never } });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
  });

  it.each([true, false])("clears the sticky identity-load error only after a successful import (%s)", async succeeds => {
    if (!succeeds) mock.importIdentity.mockRejectedValue(new Error("Invalid backup"));
    useUi.getState().toast("error", "Identity could not be loaded", { key: "identity-load", sticky: true });
    await act(async () => { useUi.getState().openSettings("identity"); root.render(<TooltipProvider><SettingsSection /></TooltipProvider>); });
    const input = container.querySelector<HTMLInputElement>('input[type="file"]')!;
    Object.defineProperty(input, "files", { configurable: true, value: [new File(["backup"], "backup.json")] });
    await act(async () => input.dispatchEvent(new Event("change", { bubbles: true })));
    expect(mock.importIdentity).toHaveBeenCalledWith("backup");
    expect(useUi.getState().toasts.some(t => t.key === "identity-load")).toBe(!succeeds);
    expect(useSettings.getState().onboarding?.completed).toBe(succeeds);
  });

  it("sends the away message only when edited here", async () => {
    await act(async () => { useUi.getState().openSettings("identity"); root.render(<TooltipProvider><SettingsSection /></TooltipProvider>); });
    const label = [...document.querySelectorAll("label")].find((l) => l.textContent?.startsWith("Away message"))!;
    const input = document.getElementById(label.htmlFor) as HTMLInputElement;
    const save = () => [...label.closest("section, div")!.parentElement!.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === "Save")!;
    await act(async () => save().click());
    expect(mock.update).not.toHaveBeenCalled();
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, " Lunch ");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await act(async () => save().click());
    expect(mock.update).toHaveBeenCalledWith({ away: "Lunch" });
  });
});
