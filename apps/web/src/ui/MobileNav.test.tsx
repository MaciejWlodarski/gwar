// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { TooltipProvider } from "./kit";
import { useMembersPanel } from "./members";
import { MoreSheet, TabBar } from "./MobileNav";
import { backDepth, backOne, useBackHistory } from "./nav";
import { SettingsSection } from "./Settings";

vi.mock("../state/controller", () => ({
  controller: { voice: { capabilities: {}, listDevices: async () => ({ inputs: [], outputs: [] }) }, getIdentity: () => new Promise(() => {}) },
  describeVoiceError: String,
  describeRequestError: String,
  identityFailure: () => null,
  bookmarkParams: () => ({}),
  UploadError: class extends Error {},
}));

const channel = (id: number, name: string): Channel => ({
  id, parent: null, name, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false,
});
const client = (id: number, channel: number | null): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel: channel as number, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null,
});
const online = (voice: number | null) =>
  useSession.setState({
    ...initialState, kind: "gwar", phase: "online", me: { session: 1, uid: "u1" }, server: { ...welcome().server, name: "Den" },
    channels: { 1: channel(1, "Lobby") }, clients: { 1: client(1, voice) },
  });

let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  // A phone's width.
  vi.stubGlobal("matchMedia", (query: string) => ({ matches: query.includes("max-width: 799px"), addEventListener() {}, removeEventListener() {} }));
  useSettings.setState({ language: "en", theme: "dark" });
  useUi.setState({ section: "home", back: "servers", settingsTab: "audio", gearTab: "audio", settingsDrill: false, pane: "list", moreOpen: false, overview: false, dialog: { kind: "none" } });
  useSession.setState({ ...initialState, phase: "idle" });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals();
});

const render = (node: React.ReactNode) => act(async () => root.render(<TooltipProvider>{node}</TooltipProvider>));
const tab = (label: string) => container.querySelector<HTMLElement>(`nav [aria-label="${label}"]`)!;
const click = (el: Element | null | undefined) => act(async () => (el as HTMLElement).click());
const tile = (label: string) => [...document.querySelectorAll<HTMLElement>('[role="dialog"] button')].find((b) => b.textContent === label);

describe("the tab bar", () => {
  it("pins Home and Servers, and holds the other sections in More", async () => {
    await render(<><TabBar /><MoreSheet /></>);
    expect([...container.querySelectorAll("nav button")].map((b) => b.getAttribute("aria-label"))).toEqual(["Home", "Servers", "More"]);
    expect(tab("Home").getAttribute("aria-current")).toBe("page");

    await click(tab("More"));
    expect(useUi.getState().moreOpen).toBe(true);
    expect(tab("More").getAttribute("aria-current")).toBe("page");
    expect(tab("Home").getAttribute("aria-current")).toBeNull();
    expect([...document.querySelectorAll('[role="dialog"] button')].map((b) => b.textContent)).toEqual(["Settings", "Report a bug", "Profile"]);

    // A pick closes the sheet; More stays marked while its section is open.
    await click(tile("Settings"));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsDrill: false, moreOpen: false });
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(tab("More").getAttribute("aria-current")).toBe("page");

    await click(tab("More"));
    await click(tile("Profile"));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "identity", settingsDrill: true, moreOpen: false });

    await click(tab("More"));
    await click(tile("Report a bug"));
    expect(useUi.getState()).toMatchObject({ section: "bugReport", moreOpen: false });
  });

  it("closes More on Esc", async () => {
    await render(<><TabBar /><MoreSheet /></>);
    await click(tab("More"));
    await act(async () => document.querySelector('[role="dialog"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState().moreOpen).toBe(false);
  });

  it("marks Servers in voice, and its tab goes back to the channel list", async () => {
    await render(<TabBar />);
    expect(tab("Servers").querySelector(".bg-ok")).toBeNull();
    await act(async () => online(1));
    expect(tab("Servers").querySelector(".bg-ok")).not.toBeNull();
    await click(tab("Servers"));
    expect(useUi.getState()).toMatchObject({ section: "servers", pane: "list" });
    await act(async () => useUi.getState().showChat());
    expect(useUi.getState().pane).toBe("chat");
    await click(tab("Servers"));
    expect(useUi.getState().pane).toBe("list");
  });
});

describe("the server section on a phone", () => {
  function Members() {
    const members = useMembersPanel();
    return <button onClick={members.toggle}>{String(members.open)}</button>;
  }

  it("opens a conversation over the channel list, and its members over it", async () => {
    online(null);
    await act(async () => useSession.getState().dispatch({ type: "selectChannel", channel: 1 }));
    expect(useUi.getState()).toMatchObject({ section: "servers", pane: "chat" });
    await render(<Members />);
    await click(container.querySelector("button"));
    expect(useUi.getState().pane).toBe("members");
    // Writing to someone from the members opens that conversation.
    await act(async () => useSession.getState().dispatch({ type: "openDm", uid: "u2", name: "Ola" }));
    expect(useUi.getState().pane).toBe("chat");
  });
});

describe("the settings list on a phone", () => {
  const row = (label: string) => [...container.querySelectorAll<HTMLElement>("main li button")].find((b) => b.textContent?.startsWith(label));

  it("lists the pages with their values, opens one over the list, and goes back", async () => {
    await act(async () => useUi.getState().openSettings());
    await render(<SettingsSection />);
    expect(container.querySelector('[role="tablist"]')).toBeNull();
    expect(row("Appearance")?.textContent).toBe("AppearanceDark");
    expect(row("Language")?.textContent).toBe("LanguageEnglish");
    expect(container.textContent).toContain("Alpha");

    await click(row("Audio"));
    expect(useUi.getState()).toMatchObject({ settingsTab: "audio", settingsDrill: true });
    expect(container.querySelector("main header h2")?.textContent).toBe("Audio");
    expect(document.activeElement?.textContent).toBe("Back");

    // Esc (or Back) leaves the page for the list first, then Settings.
    await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsDrill: false });
    expect(document.activeElement).toBe(row("Audio"));
    await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState().section).toBe("home");
  });

  it("opens the profile straight from a link", async () => {
    await act(async () => useUi.getState().openSettings("account"));
    await render(<SettingsSection />);
    expect(container.querySelector("main header h2")?.textContent).toBe("Account");
    await click(container.querySelector("main header button"));
    expect(row("Account")).toBeDefined();
  });
});

describe("back on a phone", () => {
  it("counts and takes the steps back: the sheet, the inner pages, then the page", () => {
    const ui = () => useUi.getState();
    const depth = () => backDepth(ui(), true);
    ui().showChat();
    ui().setPane("members");
    ui().setMore(true);
    expect(depth()).toBe(3);
    backOne();
    expect(ui()).toMatchObject({ moreOpen: false, pane: "members" });
    backOne();
    expect(ui().pane).toBe("chat");
    backOne();
    expect(ui().pane).toBe("list");
    expect(depth()).toBe(0);

    ui().setSection("home");
    ui().openSettings("audio");
    expect(depth()).toBe(2);
    backOne();
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    backOne();
    expect(ui().section).toBe("home");
    // Out of a server, its pages are not there to go back through.
    ui().showChat();
    expect(backDepth(ui(), false)).toBe(0);
  });

  it("keeps the steps as history entries: the system's back takes one, the app's own back drops them", async () => {
    const push = vi.spyOn(window.history, "pushState");
    const go = vi.spyOn(window.history, "go").mockImplementation(() => {});
    function Shell() {
      useBackHistory(true);
      return null;
    }
    online(null);
    await render(<Shell />);
    await act(async () => useUi.getState().showChat());
    await act(async () => useUi.getState().setPane("members"));
    expect(push).toHaveBeenCalledTimes(2);

    await act(async () => window.dispatchEvent(new PopStateEvent("popstate")));
    expect(useUi.getState().pane).toBe("chat");
    expect(go).not.toHaveBeenCalled();

    // The app's own back: the entry it leaves behind goes too, and that popstate undoes nothing more.
    await act(async () => useUi.getState().setPane("list"));
    expect(go).toHaveBeenCalledWith(-1);
    await act(async () => window.dispatchEvent(new PopStateEvent("popstate")));
    expect(useUi.getState().pane).toBe("list");
    push.mockRestore();
    go.mockRestore();
  });
});
