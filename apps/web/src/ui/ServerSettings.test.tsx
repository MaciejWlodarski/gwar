// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ update: vi.fn(async (_: object) => ({})) }));
vi.mock("../state/controller", () => ({ controller: { updateServer: mock.update, onRoster: () => () => {} }, describeRequestError: String, UploadError: class extends Error {} }));

describe("server overview", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.update.mockClear();
    useSettings.setState({ language: "en" });
    // A TeamSpeak server whose name was cleaned for display (it had bidi controls around it).
    useSession.setState({ kind: "teamspeak", channels: {}, permissions: ["manage_server"], server: { name: " Garden", welcome: "hi", default_channel: 0, max_clients: 32, teamspeak: true } as never });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const type = async (field: HTMLInputElement | HTMLTextAreaElement, value: string) => {
    await act(async () => {
      Object.getOwnPropertyDescriptor(Object.getPrototypeOf(field), "value")!.set!.call(field, value);
      field.dispatchEvent(new Event("input", { bubbles: true }));
    });
  };
  const save = async () => {
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Save")!;
    await act(async () => button.click());
  };

  it("sends the name and welcome only when edited here", async () => {
    useUi.getState().openDialog({ kind: "serverSettings" });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
    await type(document.querySelector<HTMLTextAreaElement>('[role="dialog"] textarea')!, "welcome");
    await save();
    expect(mock.update).toHaveBeenLastCalledWith({ welcome: "welcome" });
    await type(document.querySelector<HTMLInputElement>('[role="dialog"] input')!, " Yard ");
    await save();
    expect(mock.update).toHaveBeenLastCalledWith({ name: "Yard" });
  });
});
