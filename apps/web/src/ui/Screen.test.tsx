// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
import { initialScreen, useScreen } from "../state/screen";
import { useDesktopCapture, type DesktopCapture } from "../state/screen-controller";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import { TooltipProvider } from "./kit";
import { LiveBadge, ShareButton, SharingBar } from "./Screen";

const channel = (id: number, extra: Partial<Channel> = {}): Channel => ({
  id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...extra,
});
const client = (id: number, extra: Partial<Client> = {}): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true,
  voice_binding: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1 }, ...extra,
});

const getDisplayMedia = vi.fn(() => new Promise<MediaStream>(() => {}));
let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("RTCPeerConnection", class {});
  vi.stubGlobal("isSecureContext", true);
  Object.defineProperty(navigator, "mediaDevices", { configurable: true, value: { getDisplayMedia } });
  getDisplayMedia.mockClear();
  useSettings.setState({ language: "en" });
  useScreen.setState({ ...initialScreen });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
  useDesktopCapture.setState({ capture: null });
  vi.unstubAllGlobals();
});

/** Me (1) in a channel, beside a sharer (2), as the server's owner so every permission holds. */
async function show(encrypted = false) {
  useSession.setState({
    ...initialState, kind: "gwar", phase: "online", me: { session: 1, uid: "u1" }, owner: { uid: "u1" } as never,
    server: welcome().server, channels: { 1: channel(1, { encrypted }) },
    clients: { 1: client(1), 2: client(2, { sharing: true }) },
    shares: { S: { share: "S", client: 2, channel: 1, preset: "text", surface: "screen" } },
  });
  await act(async () => root.render(<TooltipProvider><div data-share-button><ShareButton /></div><LiveBadge client={useSession.getState().clients[2]!} name="n2" /></TooltipProvider>));
}
const button = () => container.querySelector<HTMLButtonElement>("[data-share-button] button");
const badge = () => container.querySelector<HTMLElement>("[title*='n2']")!;
async function desktop(capture: DesktopCapture | null) {
  (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {};
  await act(async () => useDesktopCapture.setState({ capture }));
}

describe("the Share button and LIVE badge", () => {
  it("in the browser: shares, and the badge opens the share", async () => {
    await show();
    expect(button()?.getAttribute("aria-label")).toBe("Share screen");
    expect(badge().tagName).toBe("BUTTON");
  });

  it("in the desktop app before the shell answers, and on Linux: no button, and the badge points to the browser", async () => {
    for (const capture of [null, "unsupported"] as const) {
      await desktop(capture);
      await show();
      expect(button()).toBeNull();
      expect(badge().tagName).toBe("SPAN");
      expect(badge().title).toBe("n2 is sharing their screen. Watching needs the browser for now.");
    }
  });

  it("on macOS before 14: the button says why and opens no picker; watching works", async () => {
    await desktop("macos_too_old");
    await show();
    expect(button()?.getAttribute("aria-label")).toBe("Screen sharing needs macOS 14.6 or later.");
    expect(button()?.getAttribute("aria-disabled")).toBe("true");
    await act(async () => button()!.click());
    expect(getDisplayMedia).not.toHaveBeenCalled();
    expect(badge().tagName).toBe("BUTTON");
  });

  it("on macOS 14+ and Windows: the picker opens from the click", async () => {
    await desktop("ok");
    await show();
    expect(button()?.getAttribute("aria-label")).toBe("Share screen");
    await act(async () => button()!.click());
    expect(getDisplayMedia).toHaveBeenCalledTimes(1);
    expect(useScreen.getState().own?.phase).toBe("picking");
  });

  it("shows the desktop button once the shell answers", async () => {
    await desktop(null);
    await show();
    expect(button()).toBeNull();
    await act(async () => useDesktopCapture.setState({ capture: "ok" }));
    expect(button()?.getAttribute("aria-label")).toBe("Share screen");
  });

  it("an encrypted channel says coming soon on the desktop too", async () => {
    await desktop("ok");
    await show(true);
    expect(button()?.getAttribute("aria-label")).toBe("Screen sharing in encrypted channels is coming soon.");
  });
});


it("shows independent quality controls and saves an explicit click on the surface-default mode", async () => {
  useSettings.setState({ screenMode: null, screenHeight: 1080, screenFps: 30, language: "pl" });
  useScreen.getState().dispatch({ type: "pick", attempt: 1 });
  useScreen.getState().dispatch({ type: "picked", attempt: 1, stream: {} as MediaStream, surface: "screen" });
  useScreen.getState().dispatch({ type: "starting", attempt: 1 });
  await act(async () => root.render(<SharingBar />));
  expect(container.textContent).toContain("1080p30 · Płynność");
  const modes = container.querySelectorAll<HTMLInputElement>('input[type="radio"]');
  expect(modes).toHaveLength(2);
  expect(container.textContent).toContain("Ostry tekst");
  expect(container.querySelectorAll("select")).toHaveLength(2);
  await act(async () => modes[0]!.click());
  expect(useSettings.getState().screenMode).toBe("smooth");
});
