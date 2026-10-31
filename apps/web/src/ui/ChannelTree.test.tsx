// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
import { controller } from "../state/controller";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { ChannelTree } from "./ChannelTree";
import { TooltipProvider } from "./kit";

const channel = (id: number, name: string, extra: Partial<Channel> = {}): Channel => ({
  id, parent: null, name, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...extra,
});
const client = (id: number, channel: number): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel, groups: [], platform: "ts3", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null,
});

let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en", userVolumes: {}, userMuted: {} });
  useUi.setState({ collapsed: {} });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const show = async (kind: "gwar" | "teamspeak") => {
  useSession.setState({ ...initialState, kind, phase: "online", owner: { uid: "u1" } as never, me: { session: 1, uid: "u1" }, permissions: ["connect"],
    server: { ...welcome().server, default_channel: 1 },
    channels: {
      1: channel(1, "Lobby"),
      2: channel(2, "Secret", { has_password: true }),
      3: channel(3, "Duo", { max_clients: 2 }),
      4: channel(4, "Solo", { max_clients: 1, has_password: true }),
      5: channel(5, "[cspacer1]Welcome"),
      6: channel(6, "[*spacer2]-"),
    },
    clients: { 1: client(1, 1), 7: client(7, 4) } });
  await act(async () => root.render(<TooltipProvider><ChannelTree /></TooltipProvider>));
};
const row = (name: string) => [...container.querySelectorAll<HTMLElement>('[role="treeitem"][aria-level="1"]')].find((r) => r.textContent?.includes(name))!;

it("shows how each channel can be entered, as TeamSpeak's channel icons do", async () => {
  await show("teamspeak");
  expect(row("Lobby").querySelector('[aria-label="Default channel"]')).not.toBeNull();
  expect(row("Secret").querySelector('[aria-label="Password protected"]')).not.toBeNull();
  expect(row("Duo").textContent).toContain("0/2");
  expect(row("Duo").querySelector('[aria-label="Channel is full"]')).toBeNull();
  // Full wins over the password: nobody gets in either way.
  expect(row("Solo").querySelector('[aria-label="Channel is full"]')).not.toBeNull();
  expect(row("Solo").textContent).toContain("1/1");
});

it("draws TeamSpeak spacers as spacers, and leaves such names alone on Gwar servers", async () => {
  await show("teamspeak");
  const spacer = row("Welcome");
  expect(spacer.textContent).not.toContain("[cspacer1]");
  expect(spacer.querySelector(".text-center")?.textContent).toBe("Welcome");
  expect(row("---").textContent).not.toContain("[*spacer2]");
  await act(async () => root.unmount());
  root = createRoot(container);
  await show("gwar");
  expect(row("Welcome").textContent).toContain("[cspacer1]Welcome");
});

it("joins TeamSpeak voice channels with one click or Enter, without joining spacers", async () => {
  const join = vi.spyOn(controller, "joinChannelInteractive").mockResolvedValue();
  await show("teamspeak");
  await act(async () => row("Duo").click());
  expect(join).toHaveBeenLastCalledWith(3);
  await act(async () => row("Secret").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
  expect(join).toHaveBeenLastCalledWith(2);
  join.mockClear();
  await act(async () => row("Welcome").click());
  expect(join).not.toHaveBeenCalled();
  expect(row("Welcome").querySelector('button[title="Join voice"]')).toBeNull();
});

it("opens hybrid Gwar chat safely and always exposes its Join voice action", async () => {
  const join = vi.spyOn(controller, "joinChannelInteractive").mockResolvedValue();
  await show("gwar");
  await act(async () => row("Duo").click());
  expect(join).not.toHaveBeenCalled();
  expect(useSession.getState().viewChannel).toBe(3);
  const button = row("Duo").querySelector<HTMLButtonElement>('button[title="Join voice"]')!;
  expect(button.className).not.toMatch(/opacity-0|w-0|group-hover/);
  await act(async () => button.click());
  expect(join).toHaveBeenCalledOnce();
  expect(join).toHaveBeenCalledWith(3);
});

it("shows local volume and mute independently of server mute, following the uid across sessions", async () => {
  await show("gwar");
  await act(async () => useSettings.getState().setUserVolume("u7", 0.4));
  expect(container.textContent).toContain("40%");
  await act(async () => useSettings.getState().setUserMuted("u7", true));
  expect(container.querySelector('[aria-label="Muted for me"]')).not.toBeNull();
  await act(async () => useSession.setState(s => ({ clients: { 1: s.clients[1]!, 99: { ...s.clients[7]!, id: 99, nickname: "New name" } } })));
  expect(container.querySelector('[aria-label="Muted for me"]')).not.toBeNull();
  await act(async () => container.querySelector<HTMLButtonElement>('[aria-label="Options for New name"]')!.click());
  expect(useUi.getState().dialog).toEqual({ kind: "memberProfile", uid: "u7", fallback: "New name" });
});
