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
import { SectionRail } from "./SectionRail";
import { SettingsSection } from "./Settings";
import { VoicePill } from "./VoicePanel";

const mock = vi.hoisted(() => ({ selectChannel: vi.fn(), leaveVoice: vi.fn(async () => {}), toggleMute: vi.fn(), toggleDeafen: vi.fn() }));
vi.mock("../state/controller", async () => {
  const { useSession } = await import("../state/stores");
  return {
    controller: {
      ...mock,
      // As the controller does: opening a channel's chat is a session action.
      selectChannel: (channel: number) => {
        mock.selectChannel(channel);
        useSession.getState().dispatch({ type: "selectChannel", channel });
      },
      voice: { capabilities: {}, listDevices: async () => ({ inputs: [], outputs: [] }) },
      getIdentity: () => new Promise(() => {}),
    },
    describeVoiceError: String,
    describeRequestError: String,
    identityFailure: () => null,
    bookmarkParams: () => ({}),
    UploadError: class extends Error {},
  };
});

const channel = (id: number, name: string, extra: Partial<Channel> = {}): Channel => ({
  id, parent: null, name, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...extra,
});
const client = (id: number, channel: number | null): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel: channel as number, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null,
});

/** Online on "Den", in the voice of "Games" (a sub-channel of "Lobby"), or in no channel. */
const online = (voice: number | null = 2) =>
  useSession.setState({
    ...initialState, kind: "gwar", phase: "online", me: { session: 1, uid: "u1" }, permissions: ["connect", "speak"],
    server: { ...welcome().server, name: "Den" },
    channels: { 1: channel(1, "Lobby"), 2: channel(2, "Games", { parent: 1 }) },
    clients: { 1: client(1, voice) },
  });

let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  mock.selectChannel.mockClear();
  useSettings.setState({ language: "en" });
  useUi.setState({ section: "servers", back: "servers", settingsTab: "audio", gearTab: "audio", overview: false, collapsed: {}, pane: "list", moreOpen: false, settingsDrill: false, dialog: { kind: "none" } });
  useSession.setState({ ...initialState, phase: "idle" });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals();
});

const render = (node: React.ReactNode) => act(async () => root.render(<TooltipProvider>{node}</TooltipProvider>));
const byLabel = (label: string) => container.querySelector<HTMLElement>(`[aria-label="${label}"]`);
const click = (el: HTMLElement | null) => act(async () => el!.click());
const pill = () => container.querySelector('section[aria-label="Voice connection"]');

describe("the rail's pages", () => {
  it("opens settings on the last tab with the gear, on identity with the avatar, and the bug report as pages", async () => {
    await render(<SectionRail />);
    await click(byLabel("Settings"));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "audio", back: "servers" });
    expect(byLabel("Settings")!.getAttribute("aria-current")).toBe("page");

    await click(byLabel("Your profile"));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "identity", back: "servers" });
    // The avatar stands for the profile tabs, the gear for the rest.
    expect(byLabel("Your profile")!.getAttribute("aria-current")).toBe("page");
    expect(byLabel("Settings")!.getAttribute("aria-current")).toBeNull();

    await click(byLabel("Report a bug"));
    expect(useUi.getState()).toMatchObject({ section: "bugReport", back: "servers" });
    expect(byLabel("Report a bug")!.getAttribute("aria-current")).toBe("page");
    expect(byLabel("Your profile")!.getAttribute("aria-current")).toBeNull();

    // The gear reopens its own last tab, not the profile's.
    await act(async () => useUi.getState().openSettings("appearance"));
    await click(byLabel("Your profile"));
    await click(byLabel("Settings"));
    expect(useUi.getState()).toMatchObject({ section: "settings", settingsTab: "appearance", back: "servers" });
  });

  it("marks the Servers section while in a voice channel, apart from the unread count", async () => {
    await render(<SectionRail />);
    expect(byLabel("Servers")!.querySelector(".bg-ok")).toBeNull();
    await act(async () => online());
    expect(byLabel("Servers")!.querySelector(".bg-ok")).not.toBeNull();
    await act(async () => online(null));
    expect(byLabel("Servers")!.querySelector(".bg-ok")).toBeNull();
  });
});

describe("the settings page", () => {
  const selectedTab = () => container.querySelector('[role="tab"][aria-selected="true"]')?.textContent;

  it("opens on the tab it is linked to, and follows a later link", async () => {
    await act(async () => useUi.getState().openSettings("account"));
    await render(<SettingsSection />);
    expect(selectedTab()).toBe("Account");
    await act(async () => useUi.getState().openSettings("appearance"));
    expect(selectedTab()).toBe("Appearance");
    expect(container.querySelector("main h2")?.textContent).toBe("Appearance");
  });

  it("goes back to where it was opened from on Esc, but not while typing or closing something else", async () => {
    await act(async () => useUi.getState().setSection("home"));
    await act(async () => useUi.getState().openSettings("identity"));
    await render(<SettingsSection />);
    const input = container.querySelector("input")!;
    await act(async () => input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState().section).toBe("settings");
    // A popup (here a menu) took the key; a closing tooltip, which marks it handled too, does not count.
    const handled = () => {
      const e = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
      e.preventDefault();
      return e;
    };
    // Dispatched where a key press starts, on an element, so it passes the document on its way.
    const menu = document.body.appendChild(document.createElement("div"));
    menu.setAttribute("role", "menu");
    await act(async () => document.body.dispatchEvent(handled()));
    expect(useUi.getState().section).toBe("settings");
    menu.remove();
    await act(async () => document.body.dispatchEvent(handled()));
    expect(useUi.getState().section).toBe("home");
    await act(async () => useUi.getState().openSettings("identity"));
    await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState().section).toBe("home");
  });

  it("takes focus onto the open tab, and gives it back to the gear on Esc", async () => {
    function Shell() {
      const settings = useUi((s) => s.section === "settings");
      return <><SectionRail />{settings && <SettingsSection />}</>;
    }
    await render(<Shell />);
    const gear = byLabel("Settings")!;
    gear.focus();
    await click(gear);
    expect(document.activeElement?.getAttribute("role")).toBe("tab");
    expect(document.activeElement?.getAttribute("aria-selected")).toBe("true");
    await act(async () => window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(useUi.getState().section).toBe("servers");
    expect(document.activeElement).toBe(gear);
  });
});

describe("the voice pill", () => {
  const show = async (section: Section) => {
    await act(async () => useUi.getState().setSection(section));
    await render(<VoicePill />);
  };

  it.each(["home", "settings", "bugReport"] as const)("stands in for the voice panel on %s", async (section) => {
    online();
    await show(section);
    expect(pill()).not.toBeNull();
    expect(pill()!.textContent).toContain("#Games · Den");
    for (const label of ["Mute microphone", "Deafen (mute sound and microphone)", "Leave voice"]) expect(pill()!.querySelector(`button[aria-label="${label}"]`)).not.toBeNull();
  });

  it("is not shown in the server view (overview included), nor out of voice", async () => {
    online();
    await show("servers");
    expect(pill()).toBeNull();
    await act(async () => useUi.getState().showOverview());
    expect(pill()).toBeNull();
    await act(async () => useUi.getState().setSection("home"));
    expect(pill()).not.toBeNull();
    await act(async () => online(null));
    expect(pill()).toBeNull();
    await act(async () => useSession.setState({ ...initialState, phase: "idle" }));
    expect(pill()).toBeNull();
  });

  it("says how voice is doing, as the panel does", async () => {
    online();
    await show("home");
    expect(pill()!.textContent).toContain("Connecting voice…");
    await act(async () => useSession.setState({ phase: "reconnecting" }));
    expect(pill()!.textContent).toContain("Reconnecting…");
  });

  it("leads back to the voice channel in the server view, unfolded", async () => {
    online();
    useUi.setState({ collapsed: { 1: true } });
    await show("settings");
    await click(pill()!.querySelector<HTMLElement>('button:not([aria-label])'));
    expect(mock.selectChannel).toHaveBeenCalledWith(2);
    expect(useUi.getState()).toMatchObject({ section: "servers", overview: false, collapsed: { 1: false } });
    expect(useSession.getState().viewChannel).toBe(2);
    expect(pill()).toBeNull();
  });
});
