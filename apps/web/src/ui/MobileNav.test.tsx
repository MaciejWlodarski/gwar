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
import { backDepth, navView, stepBack, useBackHistory } from "./nav";
import { SettingsSection } from "./Settings";

vi.mock("../state/controller", () => ({
  controller: { voice: { capabilities: {}, listDevices: async () => ({ inputs: [], outputs: [] }) }, getIdentity: () => new Promise(() => {}), reportDiagnostics: () => new Promise(() => {}) },
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
    const ui = () => navView(useUi.getState());
    useUi.getState().showChat();
    useUi.getState().setPane("members");
    useUi.getState().setMore(true);
    expect(backDepth(ui(), true)).toBe(3);
    const steps = [ui()];
    for (let i = 0; i < 3; i++) steps.push(stepBack(steps[i]!, true));
    expect(steps.map((v) => [v.moreOpen, v.pane])).toEqual([[true, "members"], [false, "members"], [false, "chat"], [false, "list"]]);
    expect(stepBack(steps[3]!, true)).toBe(steps[3]);

    // A page counts the steps of the view it goes back to.
    useUi.getState().setMore(false);
    useUi.getState().openSettings("audio");
    expect(backDepth(ui(), true)).toBe(4);
    expect(stepBack(stepBack(ui(), true), true)).toMatchObject({ section: "servers", pane: "members" });
    // Out of a server, its pages are not there to go back through.
    expect(backDepth({ ...ui(), section: "servers" }, false)).toBe(0);
  });
});

/**
 * Session history as a browser keeps it: a site before Gwar, then Gwar's entries. Back and Forward fire
 * popstate a moment later, as browsers do; going back past Gwar's first entry leaves the app.
 */
class FakeHistory {
  entries: Array<{ state: unknown; gwar: boolean }> = [{ state: null, gwar: false }, { state: null, gwar: true }];
  at = 1;
  left = false;
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
    const to = this.at + delta;
    if (delta === 0 || to < 0 || to >= this.entries.length) return;
    this.at = to;
    if (!this.entries[to]!.gwar) return void (this.left = true);
    setTimeout(() => window.dispatchEvent(new PopStateEvent("popstate", { state: this.state })));
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
  afterEach(() => vi.restoreAllMocks());

  function Shell({ mobile = true }: { mobile?: boolean }) {
    useBackHistory(mobile);
    return null;
  }
  // Lets a popstate fire and its effects settle.
  const settle = () => act(async () => new Promise((r) => setTimeout(r, 5)));
  const nav = async (step: () => void) => {
    await act(async () => step());
    await settle();
  };
  const ui = () => useUi.getState();

  it("Back and Forward restore the view they land on; Back from the base leaves", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    expect(history.at).toBe(3);
    await nav(() => history.back());
    expect(ui().pane).toBe("chat");
    // Forward goes forward.
    await nav(() => history.forward());
    expect(ui().pane).toBe("members");
    await nav(() => history.back());
    await nav(() => history.back());
    expect(ui().pane).toBe("list");
    expect(history.left).toBe(false);
    await nav(() => history.back());
    expect(history.left).toBe(true);
  });

  it("drops the entries the app's own way back leaves behind", async () => {
    online(null);
    await render(<Shell />);
    await nav(() => ui().showChat());
    await nav(() => ui().setPane("members"));
    await nav(() => ui().setSection("home"));
    expect(history.at).toBe(1);
    expect(ui()).toMatchObject({ section: "home", pane: "members" });
    // Its Forward entries are still there to come back to.
    await nav(() => history.forward());
    expect(ui()).toMatchObject({ section: "servers", pane: "chat" });
  });

  it("never leaves the app when the window is resized in the middle", async () => {
    await render(<Shell />);
    await nav(() => ui().setMore(true));
    expect(history.at).toBe(2);
    // The desktop closes the sheet; its Back lands on the entry below and restores it.
    await render(<Shell mobile={false} />);
    await nav(() => ui().setMore(false));
    await nav(() => history.back());
    await render(<Shell />);
    await settle();
    expect(history.left).toBe(false);
    expect(history.at).toBe(1);
    // A phone again with the sheet's entry still on top: it is dropped, nothing more.
    await nav(() => ui().setMore(true));
    await render(<Shell mobile={false} />);
    await nav(() => ui().setMore(false));
    await render(<Shell />);
    await settle();
    expect(history.left).toBe(false);
    expect(history.at).toBe(1);
    expect(ui().moreOpen).toBe(false);
  });

  it("after a reload, takes over the entries left behind without navigating, and each Back still does something", async () => {
    await render(<Shell />);
    await nav(() => ui().openSettings("audio"));
    expect(history.at).toBe(3);
    // Reload: a new page load on the same entry, its UI back at the start.
    await act(async () => root.unmount());
    useUi.setState({ section: "home", back: "servers", settingsDrill: false, moreOpen: false, pane: "list" });
    const go = vi.spyOn(history, "go");
    root = createRoot(container);
    await render(<Shell />);
    await settle();
    expect(go).not.toHaveBeenCalled();
    expect(history.at).toBe(3);
    expect(history.length).toBe(4);
    await nav(() => history.back());
    expect(ui()).toMatchObject({ section: "settings", settingsDrill: false });
    await nav(() => history.back());
    expect(ui().section).toBe("home");
    expect(history.left).toBe(false);
    await nav(() => history.back());
    expect(history.left).toBe(true);
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
