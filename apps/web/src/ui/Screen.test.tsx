// @vitest-environment happy-dom
import { act, StrictMode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { initialState } from "../state/reducer";
import { controller } from "../state/controller";
import { initialScreen, useScreen } from "../state/screen";
import { useDesktopCapture, type DesktopCapture } from "../state/screen-controller";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { TooltipProvider } from "./kit";
import * as platform from "../platform";
import { LiveBadge, ScreenNotice, ShareButton, ShareVideo, SharingBar } from "./Screen";

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

describe("persistent screen guidance", () => {
  it("keeps a localized recovery notice until the user acts", async () => {
    useSession.setState({ phase: "online", me: { session: 1, uid: "u1" }, clients: { 1: client(1) } });
    useSettings.setState({ language: "pl" });
    useScreen.setState({ notice: "reconnect" });
    await act(async () => root.render(<TooltipProvider><ScreenNotice /></TooltipProvider>));
    expect(container.querySelector('[role="alert"]')?.textContent).toContain("Udostępnianie ekranu przerwało się po utracie połączenia.");
    const restart = vi.spyOn(controller.screen, "beginShare").mockImplementation(() => {});
    const button = container.querySelector<HTMLButtonElement>('[aria-label="Udostępnij ponownie"]')!;
    await act(async () => button.click());
    expect(restart).toHaveBeenCalledOnce();
    expect(container.querySelector('[role="alert"]')).not.toBeNull();
    restart.mockRestore();
  });

  it("disables recovery sharing outside voice, without opening a picker", async () => {
    useSession.setState({ phase: "online", me: null, clients: {} });
    useSettings.setState({ language: "en" }); useScreen.setState({ notice: "reconnect" });
    await act(async () => root.render(<TooltipProvider><ScreenNotice /></TooltipProvider>));
    const restart = container.querySelector<HTMLButtonElement>('[aria-label="Share again"]')!;
    expect(restart.disabled).toBe(true);
    controller.screen.beginShare();
    expect(getDisplayMedia).not.toHaveBeenCalled();
  });

  it("uses Windows guidance for Gwar and hides its unhelpful OS shortcut", async () => {
    await desktop("ok");
    const agent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Windows NT 10.0 Chrome/153");
    useSettings.setState({ language: "pl" }); useScreen.setState({ notice: "permission" });
    await act(async () => root.render(<TooltipProvider><ScreenNotice /></TooltipProvider>));
    expect(container.textContent).toContain("Brak zgody na nagrywanie ekranu");
    expect(container.textContent).not.toContain("Ustawieniach");
    await act(async () => [...container.querySelectorAll("button")].find(b => b.textContent === "Jak włączyć")!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain("dla Gwar w Ustawieniach → Prywatność i zabezpieczenia");
    expect(dialog.textContent).not.toContain("Otwórz ustawienia");
    agent.mockRestore();
  });

  it("shows ambiguous refusal as a neutral hint and names the browser", async () => {
    const agent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Firefox/153 Linux");
    useSettings.setState({ language: "en" }); useScreen.setState({ notice: "permissionHint" });
    await act(async () => root.render(<TooltipProvider><ScreenNotice /></TooltipProvider>));
    expect(container.querySelector('[role="alert"]')).toBeNull();
    expect(container.querySelector('[role="status"]')?.textContent).toContain("Couldn't share your screen");
    await act(async () => [...container.querySelectorAll("button")].find(b => b.textContent === "How to fix")!.click());
    expect(document.querySelector('[role="dialog"]')?.textContent).toContain("If you cancelled the picker, ignore this; otherwise allow screen recording for Firefox in your system or browser privacy settings.");
    agent.mockRestore();
  });

  it("offers desktop screen settings and never displays raw browser errors", async () => {
    await desktop("ok");
    const userAgent = vi.spyOn(navigator, "userAgent", "get").mockReturnValue("Macintosh");
    useSettings.setState({ language: "pl" });
    useScreen.setState({ notice: "permission" });
    await act(async () => root.render(<TooltipProvider><ScreenNotice /></TooltipProvider>));
    expect(container.textContent).toContain("Brak zgody na nagrywanie ekranu");
    expect(container.textContent).not.toContain("Otwórz ustawienia");
    await act(async () => [...container.querySelectorAll("button")].find(b => b.textContent === "Jak włączyć")!.click());
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain("Zezwól na nagrywanie ekranu dla Gwar");
    expect(dialog.textContent).toContain("Prywatność i ochrona → Nagrywanie ekranu");
    expect(container.textContent).not.toContain("Permission denied by system");
    const settings = vi.spyOn(platform, "openScreenRecordingSettings").mockResolvedValue();
    await act(async () => [...dialog.querySelectorAll("button")].find(b => b.textContent === "Otwórz ustawienia")!.click());
    expect(settings).toHaveBeenCalledOnce();
    settings.mockRestore();
    userAgent.mockRestore();
  });
});

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
  await act(async () => [...container.querySelectorAll("button")].find(b => b.textContent === "Szczegóły")!.click());
  expect(document.body.textContent).toContain("Płynność");
  const modes = document.querySelectorAll<HTMLInputElement>('input[type="radio"]');
  expect(modes).toHaveLength(2);
  expect(document.body.textContent).toContain("Ostry tekst");
  expect(document.querySelectorAll("select")).toHaveLength(2);
  await act(async () => modes[0]!.click());
  expect(useSettings.getState().screenMode).toBe("smooth");
});

describe("watched video frames", () => {
  async function grant(run: number, id: number, stream: MediaStream, share = "A") {
    await act(async () => {
      const { dispatch } = useScreen.getState();
      dispatch({ type: "transport", run, phase: "connected" });
      dispatch({ type: "watch", share, id });
      dispatch({ type: "granted", run, id, slot: 0 });
      dispatch({ type: "stream", run, slot: 0, stream });
      root.render(<ShareVideo share={share} />);
    });
    return container.querySelector("video")!;
  }
  const visible = (video: HTMLVideoElement) => video.closest<HTMLElement>("[data-screen-video]")?.dataset.ready === "true";

  it.each([false, true])("repairs a warm receiver once, while one static frame reveals immediately (frame first: %s)", async frameFirst => {
    vi.useFakeTimers();
    const stats = vi.spyOn(controller.screen, "stats").mockResolvedValue({ width: 640, height: 400, fps: 0, framesDecoded: 1 });
    const request = vi.spyOn(controller.screen, "requestFrame").mockImplementation(() => {});
    try {
      const stream = new MediaStream();
      const video = await grant(1, 1, stream);
      if (frameFirst) await act(async () => video.dispatchEvent(new Event("loadeddata")));
      await act(async () => vi.advanceTimersByTimeAsync(100));
      expect(request.mock.calls).toEqual(frameFirst ? [] : [["A", 1, 1]]);
      expect(visible(video)).toBe(frameFirst);
      await act(async () => root.render(null));
      const moved = await grant(1, 1, stream);
      await act(async () => vi.advanceTimersByTimeAsync(1000));
      expect(request).toHaveBeenCalledTimes(frameFirst ? 0 : 1);
      // No new receiver frame/counter is required: its one loaded frame is enough.
      if (!frameFirst) await act(async () => moved.dispatchEvent(new Event("loadeddata")));
      await act(async () => vi.advanceTimersByTimeAsync(70_000));
      expect(visible(moved)).toBe(true);
      expect(request).toHaveBeenCalledTimes(frameFirst ? 0 : 1);
    } finally { stats.mockRestore(); request.mockRestore(); vi.useRealTimers(); }
  });

  it("ignores a pending warmup probe after its view unmounts", async () => {
    vi.useFakeTimers();
    let resolve!: (value: { width: number; height: number; fps: number; framesDecoded: number }) => void;
    const stats = vi.spyOn(controller.screen, "stats").mockImplementation(() => new Promise(r => { resolve = r; }));
    const request = vi.spyOn(controller.screen, "requestFrame").mockImplementation(() => {});
    try {
      await grant(1, 1, new MediaStream());
      await act(async () => vi.advanceTimersByTimeAsync(100));
      await act(async () => root.render(null));
      await act(async () => resolve({ width: 640, height: 400, fps: 0, framesDecoded: 1 }));
      expect(request).not.toHaveBeenCalled();
    } finally { stats.mockRestore(); request.mockRestore(); vi.useRealTimers(); }
  });

  it("does not mistake metadata-only resize for a loaded frame", async () => {
    const received = vi.spyOn(controller.screen, "frameReceived");
    try {
      const video = await grant(1, 1, new MediaStream());
      await act(async () => video.dispatchEvent(new Event("resize")));
      expect(visible(video)).toBe(false);
      expect(received).not.toHaveBeenCalled();
      await act(async () => video.dispatchEvent(new Event("loadeddata")));
      expect(visible(video)).toBe(true);
    } finally { received.mockRestore(); }
  });


  it("keeps the attached media player and a static frame through a view remount", async () => {
    const stream = new MediaStream();
    const video = await grant(1, 1, stream);
    await act(async () => video.dispatchEvent(new Event("loadeddata")));
    await act(async () => root.render(null));
    expect(video.srcObject).toBe(stream);
    expect(video.isConnected).toBe(true);
    expect(video.dataset.share).toBeUndefined();
    const replacement = await grant(1, 1, stream);
    expect(replacement).toBe(video);
    expect(visible(replacement)).toBe(true);
    expect(container.textContent).not.toContain("Connecting");
  });

  it("does not detach or rebind the stream during StrictMode's effect remount", async () => {
    const stream = new MediaStream();
    const video = await grant(1, 1, stream);
    const bind = vi.fn();
    Object.defineProperty(video, "srcObject", { configurable: true, get: () => stream, set: bind });
    await act(async () => root.render(<StrictMode><ShareVideo share="A" /></StrictMode>));
    expect(container.querySelector("video")).toBe(video);
    expect(bind).not.toHaveBeenCalled();
    await act(async () => video.dispatchEvent(new Event("loadeddata")));
    expect(visible(video)).toBe(true);
  });

  it("detaches an unmounted player when its transport retires", async () => {
    const stream = new MediaStream();
    const video = await grant(1, 1, stream);
    await act(async () => root.render(null));
    expect(video.srcObject).toBe(stream);
    await act(async () => useScreen.getState().dispatch({ type: "transport", run: 2, phase: "connecting" }));
    expect(video.srcObject).toBeNull();
    expect(document.querySelector("[data-screen-players]")).toBeNull();
    const replacement = await grant(2, 1, new MediaStream());
    expect(replacement).not.toBe(video);
    expect(visible(replacement)).toBe(false);
  });

  it("first use shows its first frame without waiting for another frame", async () => {
    const stream = new MediaStream();
    const video = await grant(1, 1, stream);
    expect(video.srcObject).toBe(stream);
    expect(visible(video)).toBe(false);
    await act(async () => video.dispatchEvent(new Event("loadeddata")));
    expect(visible(video)).toBe(true);
    expect(container.textContent).not.toContain("Connecting");
  });

  it("detaches the old frame on renewal and shows a static share's single fresh frame", async () => {
    vi.useFakeTimers();
    const failed = vi.spyOn(controller.screen, "frameFailed");
    try {
      const oldStream = new MediaStream();
      const old = await grant(1, 1, oldStream);
      await act(async () => old.dispatchEvent(new Event("loadeddata")));
      expect(visible(old)).toBe(true);
      // Reuse renews the run before granting the new watch, so it has no old stream.
      await act(async () => {
        useScreen.getState().dispatch({ type: "unwatch", share: "A" });
        useScreen.getState().dispatch({ type: "watch", share: "B", id: 2 });
        useScreen.getState().dispatch({ type: "transport", run: 2, phase: "connecting" });
        root.render(<ShareVideo share="B" />);
      });
      const waiting = container.querySelector("video")!;
      expect(waiting).not.toBe(old);
      expect(old.srcObject).toBeNull();
      expect(waiting.srcObject).toBeNull();
      expect(visible(waiting)).toBe(false);
      await act(async () => old.dispatchEvent(new Event("loadeddata")));
      expect(visible(waiting)).toBe(false);
      const freshStream = new MediaStream();
      const video = await grant(2, 2, freshStream, "B");
      expect(video.srcObject).toBe(freshStream);
      expect(visible(video)).toBe(false);
      // One keyframe is enough: no frame counter, timer or second frame is required.
      await act(async () => video.dispatchEvent(new Event("loadeddata")));
      expect(visible(video)).toBe(true);
      await act(async () => vi.advanceTimersByTimeAsync(35_001));
      expect(visible(video)).toBe(true);
      expect(failed).not.toHaveBeenCalled();
    } finally { failed.mockRestore(); vi.useRealTimers(); }
  });

  it("replaces the media element when the same share is rewatched in a new run", async () => {
    const old = await grant(1, 1, new MediaStream());
    await act(async () => old.dispatchEvent(new Event("loadeddata")));
    const fresh = await grant(2, 1, new MediaStream());
    expect(fresh).not.toBe(old);
    expect(old.srcObject).toBeNull();
    expect(visible(fresh)).toBe(false);
    await act(async () => fresh.dispatchEvent(new Event("loadeddata")));
    expect(visible(fresh)).toBe(true);
  });

  it("keeps another watch uninterrupted while waiting for the reused slot's fresh frame", async () => {
    vi.useFakeTimers();
    const failed = vi.spyOn(controller.screen, "frameFailed");
    try {
      const streams = [new MediaStream(), new MediaStream()];
      await act(async () => {
        const { dispatch } = useScreen.getState();
        dispatch({ type: "transport", run: 1, phase: "connected" });
        for (const [slot, share] of ["A", "B"].entries()) {
          dispatch({ type: "watch", share, id: slot + 1 });
          dispatch({ type: "granted", run: 1, id: slot + 1, slot });
          dispatch({ type: "stream", run: 1, slot, stream: streams[slot]! });
        }
        root.render(<div><ShareVideo key="A" share="A" /><ShareVideo key="B" share="B" /></div>);
      });
      const active = container.querySelector<HTMLVideoElement>('video[data-share="A"]')!;
      const old = container.querySelector<HTMLVideoElement>('video[data-share="B"]')!;
      let frame: VideoFrameRequestCallback | undefined;
      Object.defineProperties(old, {
        requestVideoFrameCallback: { configurable: true, value: (cb: VideoFrameRequestCallback) => { frame = cb; return 1; } },
        cancelVideoFrameCallback: { configurable: true, value: vi.fn() },
      });
      await act(async () => {
        active.dispatchEvent(new Event("loadeddata"));
        old.dispatchEvent(new Event("loadeddata"));
      });
      await act(async () => {
        const { dispatch } = useScreen.getState();
        dispatch({ type: "unwatch", share: "B" });
        dispatch({ type: "watch", share: "C", id: 3 });
        dispatch({ type: "granted", run: 1, id: 3, slot: 1 });
        root.render(<div><ShareVideo key="A" share="A" /><ShareVideo key="C" share="C" /></div>);
      });
      const reused = container.querySelector<HTMLVideoElement>('video[data-share="C"]')!;
      expect(container.querySelector('video[data-share="A"]')).toBe(active);
      expect(active.srcObject).toBe(streams[0]);
      expect(visible(active)).toBe(true);
      expect(reused.srcObject).toBe(streams[1]);
      expect(visible(reused)).toBe(false);
      await act(async () => reused.dispatchEvent(new Event("loadeddata")));
      expect(visible(reused)).toBe(false);
      await act(async () => frame?.(performance.now(), { receiveTime: performance.now() + 1 } as VideoFrameCallbackMetadata));
      await act(async () => vi.advanceTimersByTimeAsync(35_001));
      expect(visible(reused)).toBe(true);
      expect(visible(active)).toBe(true);
      expect(failed).not.toHaveBeenCalled();
    } finally { failed.mockRestore(); vi.useRealTimers(); }
  });

  it("reports the first frame with its run/watch ID without creating a component deadline", async () => {
    const received = vi.spyOn(controller.screen, "frameReceived");
    try {
      const video = await grant(2, 2, new MediaStream());
      expect(received).not.toHaveBeenCalled();
      expect(visible(video)).toBe(false);
      await act(async () => video.dispatchEvent(new Event("loadeddata")));
      expect(received).toHaveBeenCalledExactlyOnceWith("A", 2, 2);
      expect(visible(video)).toBe(true);
    } finally { received.mockRestore(); }
  });
});

it("shows a calm quality chip and keeps encoder statistics behind Details", async () => {
  const stats = vi.spyOn(controller.screen, "ownStats").mockResolvedValue({ width: 1920, height: 1080, fps: 60, bitrate: 4200000, codec: "VP9", encoder: "libvpx", hardware: false, limitation: "none" });
  try {
    const { dispatch } = useScreen.getState();
    dispatch({ type: "pick", attempt: 1 });
    dispatch({ type: "picked", attempt: 1, stream: {} as MediaStream, surface: "screen" });
    dispatch({ type: "starting", attempt: 1 });
    dispatch({ type: "started", attempt: 1, share: "M" });
    dispatch({ type: "live", share: "M" });
    await act(async () => root.render(<SharingBar />));
    await act(async () => [...container.querySelectorAll("button")].find(b => b.textContent === "Details")!.click());
    const details = document.querySelector<HTMLDetailsElement>('[data-testid="screen-sender-stats"]')!;
    expect(details.open).toBe(false);
    expect(details.querySelector("summary")?.textContent).toBe("Good quality");
    expect(details.textContent).toContain("1080p · 60 fps · 4.2 Mb/s · VP9");
    await act(async () => details.querySelector("summary")!.click());
    expect(details.open).toBe(true);
  } finally { stats.mockRestore(); }
});


it("opens a watched share's stage and sound controls instead of leaving the phone on its channel list", async () => {
  await show();
  useUi.setState({ pane: "list" });
  const watch = vi.spyOn(controller.screen, "watch").mockResolvedValue();
  try { await act(async () => badge().click()); expect(useUi.getState().pane).toBe("chat"); expect(watch).toHaveBeenCalledWith("S"); }
  finally { watch.mockRestore(); }
});
