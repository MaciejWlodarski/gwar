// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ join: vi.fn(async (_channel: number, _password?: string) => true) }));
vi.mock("../state/controller", () => ({ controller: { joinChannel: mock.join, onRoster: () => () => {} }, describeJoinError: String, describeRequestError: String, UploadError: class extends Error {} }));

describe("the channel password dialog", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.join.mockReset();
    useSettings.setState({ language: "en" });
    useSession.setState({ kind: "teamspeak", channels: {} });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const submit = async (password: string) => {
    const input = document.querySelector<HTMLInputElement>('[role="dialog"] input[type="password"]')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, password);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Join channel")!;
    await act(async () => button.click());
  };

  it("takes the password again when a reconnect ended the first join", async () => {
    useUi.getState().openDialog({ kind: "channelPassword", channel: 3 });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
    // The join belonged to the session before the resync, which keeps this dialog open.
    mock.join.mockResolvedValueOnce(false);
    await submit("pw");
    expect(useUi.getState().dialog.kind).toBe("channelPassword");
    mock.join.mockResolvedValueOnce(true);
    await submit("pw");
    expect(mock.join.mock.calls).toEqual([[3, "pw"], [3, "pw"]]);
    expect(useUi.getState().dialog.kind).toBe("none");
  });
});
