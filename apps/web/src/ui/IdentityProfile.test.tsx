// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { SettingsSection } from "./Settings";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ update: vi.fn(async (_: object) => ({})) }));
vi.mock("../state/controller", () => ({
  controller: { updateProfile: mock.update, getIdentity: () => new Promise(() => {}) },
  describeVoiceError: String,
  UploadError: class extends Error {},
}));

describe("profile", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.update.mockClear();
    useSettings.setState({ language: "en" });
    // A TeamSpeak away message cleaned for display (it ended in a bidi control).
    useSession.setState({ phase: "online", me: { session: 1, uid: "ts:me" } as never, clients: { 1: { id: 1, uid: "ts:me", away: "Busy" } as never } });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
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
