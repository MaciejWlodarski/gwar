// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({
  description: vi.fn(async (_channel: number) => ""),
  poke: vi.fn(async (_client: number, _uid: string, _message: string) => ({})),
  ban: vi.fn(async (_ban: object) => ({})),
}));
vi.mock("../state/controller", () => ({
  controller: { channelDescription: mock.description, pokeClient: mock.poke, createBan: mock.ban, onRoster: () => () => {} },
  describeJoinError: String,
  describeRequestError: String,
  UploadError: class extends Error {},
}));

const channel = { id: 5, parent: null, name: "Rules", topic: "Read me", position: 0, has_password: true, max_clients: 10, max_family_clients: 25, encrypted: false, history: false, private: false, synced: false };

describe("TeamSpeak dialogs", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.description.mockReset();
    mock.poke.mockClear();
    mock.ban.mockClear();
    useSettings.setState({ language: "en" });
    useSession.setState({ kind: "teamspeak", channels: { 5: channel }, members: {} });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const open = async () => act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));

  it("shows a channel's description as text, never as markup", async () => {
    mock.description.mockResolvedValueOnce("<img src=x onerror=alert(1)>\nline two");
    useUi.getState().openDialog({ kind: "channelInfo", channel: 5 });
    await open();
    const shown = document.querySelector('[data-testid="channel-description"]')!;
    expect(shown.textContent).toBe("<img src=x onerror=alert(1)>\nline two");
    expect(shown.querySelector("img")).toBeNull();
    expect(mock.description).toHaveBeenCalledWith(5);
    const text = document.querySelector('[role="dialog"]')!.textContent!;
    for (const part of ["Read me", "10", "25", "Required"]) expect(text).toContain(part);
  });

  it("pokes with the typed message", async () => {
    useUi.getState().openDialog({ kind: "poke", person: { uid: "ts:b", nickname: "bob", session: 7 } });
    await open();
    const input = document.querySelector<HTMLInputElement>('[role="dialog"] input')!;
    expect(input.maxLength).toBe(100);
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "look up");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find((b) => b.textContent === "Poke")!;
    await act(async () => button.click());
    expect(mock.poke).toHaveBeenCalledWith(7, "ts:b", "look up");
    expect(useUi.getState().dialog.kind).toBe("none");
  });

  it("shows a description without formatting that would reorder it, keeping its lines", async () => {
    mock.description.mockResolvedValueOnce("Rules\u202e1\n\tbe nice\u0000");
    useUi.getState().openDialog({ kind: "channelInfo", channel: 5 });
    await open();
    expect(document.querySelector('[data-testid="channel-description"]')!.textContent).toBe("Rules1\n\tbe nice");
  });

  it("bans the person the dialog was opened for, by session and identity", async () => {
    useUi.getState().openDialog({ kind: "ban", person: { uid: "ts:alice", nickname: "alice", session: 7 } });
    await open();
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find((b) => b.textContent === "Ban…")!;
    await act(async () => button.click());
    expect(mock.ban).toHaveBeenCalledWith(expect.objectContaining({ client: 7, uid: "ts:alice" }));
  });
});
