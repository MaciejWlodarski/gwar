// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
import { useSettings } from "../state/settings";
import { useSession, useUi, type Section } from "../state/stores";
import { TooltipProvider } from "./kit";
import { useMembersPanel } from "./members";
import { MoreSheet, TabBar } from "./MobileNav";
import { BugReportSection } from "./BugReport";
import { backInApp } from "./hooks";
import { backDepth, LOST_POP_MS, navView, paneBack, readView, showVoiceChannel, stepBack, useBackHistory } from "./nav";
import { clearReportDraft } from "./report-draft";
import { SettingsSection } from "./Settings";

vi.mock("../state/controller", () => ({
  controller: { voice: { capabilities: {}, micPermission: async () => "prompt", listDevices: async () => ({ inputs: [], outputs: [] }) }, getIdentity: () => new Promise(() => {}), reportDiagnostics: () => new Promise(() => {}), selectChannel: () => {} },
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
  clearReportDraft();
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

    await click(row("Voice"));
    expect(useUi.getState()).toMatchObject({ settingsTab: "audio", settingsDrill: true });
    expect(container.querySelector("main header h2")?.textContent).toBe("Voice");
    expect(document.activeElement?.textContent).toBe("Back");

    // Esc (or Back) leaves the page for the list first, then Settings.
    await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsDrill: false });
    expect(document.activeElement).toBe(row("Voice"));
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
  it("counts and takes the steps back: a settings tab, the page, the members, the conversation", () => {
    const ui = () => navView(useUi.getState());
    useUi.getState().showChat();
    useUi.getState().setPane("members");
    expect(backDepth(ui(), true)).toBe(2);
    const steps = [ui()];
    for (let i = 0; i < 2; i++) steps.push(stepBack(steps[i]!, true));
    expect(steps.map((v) => v.pane)).toEqual(["members", "chat", "list"]);
    expect(stepBack(steps[2]!, true)).toBe(steps[2]);
    // A page counts the steps of the view it goes back to.
    useUi.getState().openSettings("audio");
    expect(backDepth(ui(), true)).toBe(4);
    expect(stepBack(stepBack(ui(), true), true)).toMatchObject({ section: "servers", pane: "members" });
    // Out of a server, its pages are not there to go back through.
    expect(backDepth({ ...ui(), section: "servers" }, false)).toBe(0);
  });
});

describe("a history entry's view", () => {
  const view = (v: unknown) => readView({ gwarNav: { id: "x", view: v } });

  it("is rebuilt from the known fields, each from its own values", () => {
    expect(readView(null)).toBeNull();
    expect(readView({ gwarNav: "settings" })).toBeNull();
    expect(view({ settingsTab: "audio" })).toBeNull();
    expect(view({ section: "admin" })).toBeNull();
    const full = { section: "settings", back: "home", pane: "chat", settingsTab: "language", settingsDrill: true };
    expect(view(full)).toEqual(full);
    // A retired or crafted tab is the first one; a page can't be where a page goes back to.
    expect(view({ ...full, settingsTab: "retired-tab" })?.settingsTab).toBe("audio");
    expect(view({ ...full, settingsTab: 'x"]???' })?.settingsTab).toBe("audio");
    expect(view({ ...full, back: "settings", pane: "lobby", settingsDrill: "yes" })).toMatchObject({ back: "servers", pane: "list", settingsDrill: false });
    // Nothing else comes along.
    expect(Object.keys(view({ ...full, setSection: null, moreOpen: true, __proto__: { polluted: 1 } })!).sort()).toEqual(Object.keys(full).sort());
  });
});

/**
 * Session history as a browser keeps it: a site before Gwar, then Gwar's entries. Back and Forward traverse a
 * moment later, as browsers do, and fire popstate as they land (each one in turn, from where the one before landed);
 * going back past Gwar's first entry leaves the app. `dropNext` loses the next traversal's popstate (the traversal
 * still happens). It notes any navigation started while a popstate is being handled: there should be none.
 */
class FakeHistory {
  entries: Array<{ state: unknown; gwar: boolean }> = [{ state: null, gwar: false }, { state: null, gwar: true }];
  at = 1;
  left = false;
  inPopstate = false;
  navigatedInPopstate = 0;
  dropNext = false;
  get state() {
    return this.entries[this.at]!.state;
  }
  get length() {
    return this.entries.length;
  }
  pushState(state: unknown) {
    this.entries.splice(this.at + 1, Infinity, { state: structuredClone(state), gwar: true });
    this.at++;
  }
  replaceState(state: unknown) {
    this.entries[this.at]!.state = structuredClone(state);
  }
  go(delta: number) {
    if (this.inPopstate) this.navigatedInPopstate++;
    const drop = this.dropNext;
    this.dropNext = false;
    setTimeout(() => {
      const to = this.at + delta;
      if (this.left || delta === 0 || to < 0 || to >= this.entries.length) return;
      this.at = to;
      if (!this.entries[to]!.gwar) return void (this.left = true);
      if (drop) return;
      this.inPopstate = true;
      window.dispatchEvent(new PopStateEvent("popstate", { state: this.state }));
      this.inPopstate = false;
    });
  }
  back() {
    this.go(-1);
  }
  forward() {
    this.go(1);
  }
}

describe("phone steps as browser history", () => {
  let history: FakeHistory;
  beforeEach(() => {
    history = new FakeHistory();
    vi.spyOn(window, "history", "get").mockReturnValue(history as unknown as History);
  });
  afterEach(() => {
    expect(history.navigatedInPopstate).toBe(0);
    vi.restoreAllMocks();
  });

  function Shell({ mobile = true, children }: { mobile?: boolean; children?: React.ReactNode }) {
    useBackHistory(mobile);
    return <>{children}</>;
  }
  // Lets a popstate fire and its effects settle.
  const settle = () => act(async () => new Promise((r) => setTimeout(r, 5)));
  const nav = async (step: () => void) => {
    await act(async () => step());
    await settle();
  };
  const ui = () => useUi.getState();
  const reload = async (node: React.ReactNode = <Shell />) => {
    await act(async () => root.unmount());
    useUi.setState({ section: "servers", back: "servers", settingsTab: "audio", settingsDrill: false, moreOpen: false, pane: "list" });
    root = createRoot(container);
    await render(node);
    await settle();
  };

  it("Back and Forward apply the view they land on; Back from the base leaves", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    expect(history.at).toBe(3);
    await nav(() => history.back());
    expect(ui().pane).toBe("chat");
    await nav(() => history.forward());
    expect(ui().pane).toBe("members");
    await nav(() => history.back());
    await nav(() => history.back());
    expect(ui().pane).toBe("list");
    expect(history.left).toBe(false);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("an in-app way back takes the entry this load pushed with it, one step", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => paneBack("chat"));
    expect([history.at, ui().pane]).toEqual([2, "chat"]);
    await nav(() => paneBack("list"));
    expect([history.at, ui().pane]).toEqual([1, "list"]);
    // What it went back from is Forward of here.
    await nav(() => history.forward());
    expect(ui().pane).toBe("chat");
    // A tab back to a top from two steps in: one entry back, which now holds the top.
    await nav(() => ui().setPane("members"));
    await nav(() => backInApp(() => ui().setSection("home")));
    expect(history.at).toBe(2);
    expect(readView(history.state)?.section).toBe("home");
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "servers", pane: "list" });
    expect(history.left).toBe(false);
  });

  it("never goes back over an entry it did not push: a crafted one stays where it is", async () => {
    online(null);
    const go = vi.spyOn(history, "go");
    await render(<Shell />);
    // Another script's entry, claiming to be two steps in; the app's own steps go on top of it.
    history.pushState({ gwarNav: { id: "foreign", index: 2, view: { section: "servers", pane: "list" } } });
    await nav(() => ui().showChat());
    expect(history.at).toBe(3);
    await nav(() => history.back());
    expect(ui().pane).toBe("list");
    await nav(() => ui().showChat());
    await nav(() => paneBack("list"));
    await nav(() => backInApp(() => ui().setSection("home")));
    expect([history.at, history.left, ui().section]).toEqual([2, false, "home"]);
    expect(go.mock.calls.every(([delta]) => Math.abs(delta) === 1)).toBe(true);
  });

  it("takes only checked fields from an entry: no unknown tab, no store field from outside", async () => {
    await render(<Shell><Page /></Shell>);
    await nav(() => ui().openSettings());
    const setSection = ui().setSection;
    history.pushState({ gwarNav: { id: "x", view: { section: "settings", settingsTab: 'x"]???', settingsDrill: true, setSection: null } } });
    history.pushState({ gwarNav: { id: "y", view: { section: "nowhere" } } });
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "settings", settingsTab: "audio", settingsDrill: true });
    expect(ui().setSection).toBe(setSection);
    expect(container.querySelector("main header h2")?.textContent).toBe("Voice");
    // An entry without a view of ours is the base view.
    await nav(() => history.forward());
    expect(ui()).toMatchObject({ section: "servers", pane: "list" });
  });

  it("a reload shows the page its entry holds, and Back goes through the earlier pages", async () => {
    await render(<Shell />);
    await nav(() => ui().openSettings("audio"));
    expect(history.length).toBe(4);
    const go = vi.spyOn(history, "go");
    await reload();
    expect(go).not.toHaveBeenCalled();
    expect(ui()).toMatchObject({ section: "settings", settingsTab: "audio", settingsDrill: true });
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    await nav(() => history.back());
    expect(ui().section).toBe("home");
    expect(history.left).toBe(false);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("after a reload an in-app way back rewrites the entry instead of going back, so Back shows that page once more", async () => {
    await render(<Shell />);
    await nav(() => ui().openSettings("audio"));
    await reload();
    const go = vi.spyOn(history, "go");
    const length = history.length;
    await nav(() => backInApp(() => ui().setSettingsDrill(false)));
    // The reloaded entry is no entry of this load's: the step back rewrites it, it does not go back in history.
    expect(go).not.toHaveBeenCalled();
    expect(history.length).toBe(length);
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    // The accepted cost: the entry below holds the same page, so the first Back shows it again.
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    await nav(() => history.back());
    expect(ui().section).toBe("home");
    expect(history.left).toBe(false);
  });

  it("the More sheet is no entry: opening it and reloading, again and again, adds nothing", async () => {
    await render(<Shell />);
    await nav(() => ui().openSettings());
    const length = history.length;
    for (let i = 0; i < 3; i++) {
      await nav(() => ui().setMore(true));
      expect(history.length).toBe(length);
      await reload();
      expect(ui()).toMatchObject({ section: "settings", moreOpen: false });
    }
    expect(history.length).toBe(length);
    // Back from it is the page below, and then the app is left.
    await nav(() => ui().setMore(true));
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "home", moreOpen: false });
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("never navigates when the window is resized in the middle", async () => {
    await render(<Shell />);
    await nav(() => ui().openSettings("audio"));
    await nav(() => ui().setMore(true));
    // The desktop closes the sheet and only applies what Back lands on.
    await render(<Shell mobile={false} />);
    await nav(() => ui().setMore(false));
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    await nav(() => ui().setSection("home"));
    const at = history.at;
    await render(<Shell />);
    await settle();
    expect([history.at, history.left]).toEqual([at, false]);
  });

  it("leaves a page on Back as its own way back does, giving focus back to what opened it", async () => {
    await render(<><button id="opener">open</button><Shell /><Page /></>);
    const opener = container.querySelector<HTMLElement>("#opener")!;
    opener.focus();
    await nav(() => ui().openSettings());
    container.querySelector<HTMLElement>('[data-page="language"]')!.focus();
    await nav(() => history.back());
    expect(ui().section).toBe("home");
    expect(document.activeElement).toBe(opener);
  });

  it("writing to someone from the members goes back to the conversation's entry: no dead Back after it", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    expect(history.at).toBe(3);
    // Not an in-app way back, yet a step back: the members' entry goes with it.
    await nav(() => useSession.getState().dispatch({ type: "openDm", uid: "u2", name: "Ola" }));
    expect([history.at, ui().pane]).toEqual([2, "chat"]);
    await nav(() => paneBack("list"));
    expect([history.at, ui().pane]).toEqual([1, "list"]);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("a notification opening a conversation over the members goes back the same way, and Back shows the list", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => useSession.getState().dispatch({ type: "setActive", key: "server" }));
    expect([history.at, ui().pane]).toEqual([2, "chat"]);
    await nav(() => history.back());
    expect([ui().pane, history.left]).toEqual(["list", false]);
  });

  it("the voice pill from a settings tab goes back over every entry down to the channel list, in one go", async () => {
    online(1);
    const go = vi.spyOn(history, "go");
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().openSettings("audio"));
    expect(history.at).toBe(4);
    await nav(() => showVoiceChannel());
    await settle();
    expect([history.at, ui().section, ui().pane]).toEqual([1, "servers", "list"]);
    expect(go.mock.calls.map(([delta]) => delta)).toEqual([-3]);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("a second way back while the first is under way is taken too: no entry is left twice", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    // Two quick taps: the second comes before the first one's popstate.
    await nav(() => {
      paneBack("chat");
      paneBack("list");
    });
    await settle();
    expect([history.at, ui().pane]).toEqual([1, "list"]);
    await nav(() => history.forward());
    expect(ui().pane).toBe("chat");
    // Not a step back to where the first one goes: a tab to Home, taken from the entry below, which then holds it.
    await nav(() => ui().setPane("members"));
    await nav(() => {
      paneBack("chat");
      backInApp(() => ui().setSection("home"));
    });
    await settle();
    expect([history.at, readView(history.state)?.section]).toEqual([1, "home"]);
  });

  it("Back closes a dialog along with the view it was opened over", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().openDialog({ kind: "channelPassword", channel: 1 }));
    await nav(() => history.back());
    expect([ui().pane, ui().dialog.kind]).toEqual(["list", "none"]);
    // A dialog over the same view (an entry that holds it again) stays.
    await nav(() => history.forward());
    await nav(() => ui().openDialog({ kind: "channelPassword", channel: 1 }));
    await nav(() => ui().setPane("members"));
    await nav(() => history.back());
    expect([ui().pane, ui().dialog.kind]).toEqual(["chat", "none"]);
  });

  it("a quiet reconnect (a renewed certificate) leaves Settings as it is: no landing, no history change", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => ui().openSettings("account"));
    const go = vi.spyOn(history, "go");
    const [at, length] = [history.at, history.length];
    // As `replaceIdentityQuietly` does it: the session drops and connects again, with nobody asking to go anywhere.
    await nav(() => useSession.setState({ ...initialState, phase: "idle" }));
    await nav(() => useSession.getState().dispatch({ type: "phase", phase: "connecting" }));
    await nav(() => online(null));
    expect(ui()).toMatchObject({ section: "settings", settingsTab: "account", settingsDrill: true, back: "servers", pane: "members" });
    expect(go).not.toHaveBeenCalled();
    expect([history.at, history.length]).toEqual([at, length]);
    // Every Back is a step of its own: the list of tabs, the members, the conversation, the channel list.
    const seen = [];
    for (let i = 0; i < 4; i++) {
      await nav(() => history.back());
      seen.push(`${ui().section}/${ui().section === "settings" ? String(ui().settingsDrill) : ui().pane}`);
    }
    expect(seen).toEqual(["settings/false", "servers/members", "servers/chat", "servers/list"]);
    expect(history.left).toBe(false);
  });

  it("connecting again to the same server after a drop opens its channel list, not the pane it was left on", async () => {
    online(null);
    useSession.setState({ address: "den.example" });
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    // Dropped (the address stays), then Connect to the same address: `connectInteractive` lands first.
    await nav(() => useSession.setState({ ...initialState, phase: "idle" }));
    expect(useSession.getState().address).toBe("den.example");
    await nav(() => ui().landOnServer());
    await nav(() => useSession.getState().dispatch({ type: "phase", phase: "connecting" }));
    await nav(() => online(null));
    expect(ui()).toMatchObject({ section: "servers", pane: "list" });
  });

  it("a reload restores the pane its entry holds, and connecting then opens the channel list without navigating", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    useSession.setState({ ...initialState, phase: "idle" });
    await reload();
    expect(ui().pane).toBe("members");
    const go = vi.spyOn(history, "go");
    const length = history.length;
    await nav(() => ui().landOnServer());
    await nav(() => useSession.getState().dispatch({ type: "phase", phase: "connecting" }));
    await nav(() => online(null));
    expect(ui()).toMatchObject({ section: "servers", pane: "list" });
    expect(go).not.toHaveBeenCalled();
    expect([history.length, readView(history.state)?.pane]).toEqual([length, "list"]);
  });

  it("a notification over a settings tab goes back to the conversation's entry in one go, never below where it goes", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => ui().openSettings("audio"));
    expect(history.at).toBe(5);
    const go = vi.spyOn(history, "go");
    await nav(() => useSession.getState().dispatch({ type: "setActive", key: "server" }));
    expect([history.at, ui().section, ui().pane]).toEqual([2, "servers", "chat"]);
    expect(go.mock.calls.map(([delta]) => delta)).toEqual([-3]);
    // What it went back over is Forward of here.
    await nav(() => history.forward());
    expect(ui().pane).toBe("members");
  });

  it("a user's Back racing our way back lands one entry further, still in the app, showing that entry", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => ui().openSettings("audio"));
    // Before our step lands, the user's Back comes: both traverse, ours first.
    await nav(() => {
      useSession.getState().dispatch({ type: "setActive", key: "server" });
      history.back();
    });
    await settle();
    expect([history.at, history.left, ui().section, ui().pane]).toEqual([1, false, "servers", "list"]);
    expect(readView(history.state)).toEqual(navView(ui()));
    // The user's Back first, then our step: it still ends on an entry, showing what it holds.
    await nav(() => history.forward());
    await nav(() => ui().setPane("members"));
    expect(history.at).toBe(3);
    await nav(() => {
      history.back();
      paneBack("chat");
    });
    await settle();
    expect([history.at, history.left]).toEqual([1, false]);
    expect(navView(ui())).toEqual(readView(history.state));
  });

  it("a popstate that never comes stops being awaited after a while: the next way back goes again", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    history.dropNext = true;
    await nav(() => paneBack("chat"));
    expect([history.at, ui().pane]).toEqual([2, "chat"]);
    // Meanwhile a way back waits for it.
    await nav(() => paneBack("list"));
    expect(history.at).toBe(2);
    await act(async () => new Promise((r) => setTimeout(r, LOST_POP_MS + 50)));
    await settle();
    // Given up on: the entry holds what the app shows, and the steps go on.
    expect(readView(history.state)).toEqual(navView(ui()));
    await nav(() => ui().showChat());
    await nav(() => paneBack("list"));
    expect([history.at, ui().pane, history.left]).toEqual([2, "list", false]);
  });

  it("coming back from the back/forward cache stops awaiting a popstate the page was left with", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    history.dropNext = true;
    await nav(() => paneBack("chat"));
    await nav(() => window.dispatchEvent(Object.assign(new Event("pageshow"), { persisted: true })));
    await nav(() => paneBack("list"));
    expect([history.at, ui().pane]).toEqual([1, "list"]);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("focus follows the server panes on Back: the conversation's way back, then the open channel's row", async () => {
    // The panes as the server section shows them on a phone, marked as the real ones are.
    function Panes() {
      const pane = useUi((s) => s.pane);
      if (pane === "list") return <div role="tree"><div role="treeitem" tabIndex={0}>General</div><div role="treeitem" tabIndex={0} data-entry>Lobby</div></div>;
      if (pane === "chat") return <main><button data-entry>Back to channels</button><textarea /></main>;
      return <aside><button data-entry>Back</button></aside>;
    }
    online(null);
    await render(<Shell><Panes /></Shell>);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => history.back());
    expect(document.activeElement?.textContent).toBe("Back to channels");
    await nav(() => history.back());
    expect(document.activeElement?.textContent).toBe("Lobby");
    // The in-app buttons go the same way.
    await nav(() => history.forward());
    await nav(() => paneBack("list"));
    expect(document.activeElement?.textContent).toBe("Lobby");
  });
});

/** The open section's page, as App shows it on a phone. */
function Page() {
  const section = useUi((s) => s.section);
  const pages: Partial<Record<Section, React.ReactNode>> = { settings: <SettingsSection />, bugReport: <BugReportSection /> };
  return <>{pages[section]}</>;
}

describe("the More sheet over a page", () => {
  // As a real key press: cancelable, so the sheet can mark it handled. Radix moves focus on close a moment later.
  const esc = async () => {
    await act(async () => document.activeElement!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true })));
    await act(async () => new Promise((r) => setTimeout(r, 5)));
  };
  const pick = async (label: string) => {
    await click(tile(label));
    await act(async () => new Promise((r) => setTimeout(r, 5)));
  };
  const more = () => container.querySelector<HTMLElement>("[data-more]")!;

  it("is modal, and Esc closes only the sheet: the page and its draft stay, focus goes back to More", async () => {
    await act(async () => useUi.getState().setSection("bugReport"));
    await render(<><Page /><TabBar /><MoreSheet /></>);
    const text = container.querySelector("textarea")!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(text, "it broke");
      text.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await click(more());
    expect(document.querySelector('[role="dialog"]')?.getAttribute("aria-modal")).toBe("true");
    await esc();
    expect(useUi.getState()).toMatchObject({ moreOpen: false, section: "bugReport" });
    expect(container.querySelector("textarea")?.value).toBe("it broke");
    expect(document.activeElement).toBe(more());
  });

  it("Esc over a settings page keeps that page open", async () => {
    await act(async () => useUi.getState().openSettings("audio"));
    await render(<><Page /><TabBar /><MoreSheet /></>);
    await click(more());
    await esc();
    expect(useUi.getState()).toMatchObject({ moreOpen: false, section: "settings", settingsDrill: true });
  });

  it("a picked tile hands focus to the page it opens", async () => {
    await render(<><Page /><TabBar /><MoreSheet /></>);
    await click(more());
    await pick("Report a bug");
    expect(document.activeElement?.tagName).toBe("TEXTAREA");
    await click(more());
    await pick("Settings");
    expect(document.activeElement?.getAttribute("data-page")).toBe("audio");
    await click(more());
    await pick("Profile");
    expect(document.activeElement?.textContent).toBe("Back");
  });
});
