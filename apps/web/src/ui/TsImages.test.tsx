// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Client } from "../proto/Client";
import type { ServerInfo } from "../proto/ServerInfo";
import { useTsImages } from "../net/ts-images";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import { TsGroupBadges, TsIcon } from "./badges";
import { Avatar } from "./kit";

const AVATAR = `a${"ab".repeat(20)}`;
const server: ServerInfo = {
  name: "TS", welcome: "", version: "3", default_channel: 1, max_clients: 32, upload_limit: 0, file_access: "link", teamspeak: true, screen_guest_links: true,
  server_chat: { encrypted: false, history: false }, icon: "i1001",
  badges: [
    { kind: "server", id: 6, name: "Server Admin", sort: 0 },
    { kind: "server", id: 9, name: "Iconic", sort: 5, icon: "i412469435" },
  ],
};
const bob: Client = {
  id: 2, uid: "ts:Ym9i", nickname: "bob", channel: 1, groups: [6, 9], platform: "ts3", muted: false, deafened: false, away: null,
  talking: false, voice: true, voice_binding: null, avatar: AVATAR,
};

describe("TeamSpeak icons and avatars", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    useSession.setState({ kind: "teamspeak", server, clients: { 2: bob } });
    useTsImages.setState({ urls: { i412469435: "blob:gwar/icon", [AVATAR]: "blob:gwar/avatar", i1001: "blob:gwar/server" } });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
    useTsImages.setState({ urls: {} });
  });

  const images = () => [...container.querySelectorAll("img")].map((i) => i.getAttribute("src"));

  it("shows a member's TeamSpeak avatar in place of the initials", async () => {
    await act(async () => root.render(<Avatar name="bob" seed={bob.uid} uid={bob.uid} />));
    expect(images()).toEqual(["blob:gwar/avatar"]);
    expect(container.textContent).toBe("");
  });

  it("keeps the initials until the avatar is loaded, for people without one, and on a Gwar server", async () => {
    useTsImages.setState({ urls: {} });
    await act(async () => root.render(<Avatar name="bob" uid={bob.uid} />));
    expect(images()).toEqual([]);
    expect(container.textContent).toBe("BO");
    useTsImages.setState({ urls: { [AVATAR]: "blob:gwar/avatar" } });
    await act(async () => root.render(<Avatar name="alice" uid="ts:YWxpY2U=" />));
    expect(images()).toEqual([]);
    useSession.setState({ kind: "gwar" });
    await act(async () => root.render(<Avatar name="bob" uid={bob.uid} />));
    expect(images()).toEqual([]);
  });

  it("falls back to the initials when the picture does not load", async () => {
    await act(async () => root.render(<Avatar name="bob" uid={bob.uid} />));
    await act(async () => container.querySelector("img")!.dispatchEvent(new Event("error")));
    expect(images()).toEqual([]);
    expect(container.textContent).toBe("BO");
  });

  it("shows group icons in the badges, and none for built-in or missing ones", async () => {
    await act(async () => root.render(<TsGroupBadges groups={bob.groups} channelGroup={null} />));
    const badges = [...container.querySelectorAll("[data-testid=ts-group-badges] > span")];
    expect(badges.map((b) => b.textContent)).toEqual(["Server Admin", "Iconic"]);
    expect(badges.map((b) => b.querySelector("img")?.getAttribute("src") ?? null)).toEqual([null, "blob:gwar/icon"]);
  });

  it("draws an icon only once it is loaded", async () => {
    await act(async () => root.render(<><TsIcon token="i1001" /><TsIcon token="i5" /><TsIcon token={null} /></>));
    expect(images()).toEqual(["blob:gwar/server"]);
    expect(container.querySelector("img")?.getAttribute("alt")).toBe("");
  });
});
