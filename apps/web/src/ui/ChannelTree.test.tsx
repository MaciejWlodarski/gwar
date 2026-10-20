// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
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
  useSettings.setState({ language: "en" });
  useUi.setState({ collapsed: {} });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals();
});

const show = async (kind: "gwar" | "teamspeak") => {
  useSession.setState({ ...initialState, kind, phase: "online", me: { session: 1, uid: "u1" }, permissions: ["connect"],
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
