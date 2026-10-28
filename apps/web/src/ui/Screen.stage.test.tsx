// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Link } from "../net/connection";
import { welcome } from "../net/testing";
import { controller } from "../state/controller";
import { initialScreen, useScreen } from "../state/screen";
import { ScreenController } from "../state/screen-controller";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import type { WatchParams } from "../voice/screen";
import { Stage } from "./Screen";
import { TooltipProvider } from "./kit";

vi.mock("../voice/screen", async importOriginal => {
  const actual = await importOriginal<typeof import("../voice/screen")>();
  class ScreenWatchTransport {
    readonly run: number;
    constructor(readonly params: WatchParams) { this.run = params.run; }
    async start() { await this.params.offer("watch-offer", this.run); this.params.state("connected"); }
    async stats() { return null; }
    close = vi.fn();
    clearSlot = vi.fn();
    keyframe = vi.fn();
  }
  return { ...actual, ScreenWatchTransport };
});

const originalScreen = controller.screen;
let screen: ScreenController, root: Root, container: HTMLDivElement;
const request = vi.fn(async (op: string, d: { share?: string }) => {
  if (op === "screen.watch.offer") return { sdp: "answer" };
  if (op === "screen.watch") return { slot: d.share === "A" ? 0 : 1 };
  return {};
});

beforeEach(async () => {
  vi.useFakeTimers();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("RTCPeerConnection", class {});
  request.mockClear();
  const link = { request } as unknown as Link;
  screen = new ScreenController(() => link, String);
  Object.defineProperty(controller, "screen", { value: screen });
  useSettings.setState({ language: "en" });
  useUi.setState({ toast: vi.fn() });
  useScreen.setState({ ...initialScreen });
  useSession.getState().dispatch({
    type: "welcome", resync: false, now: 1,
    welcome: welcome({ shares: ["A", "B", "C"].map(share => ({ share, client: 2, channel: 1, preset: "text", surface: "screen" })) }),
  });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    await screen.watch("A");
    await screen.watch("B");
    const { dispatch, transport, watches } = useScreen.getState();
    expect(watches).toMatchObject([
      { share: "A", status: "watching" }, { share: "B", status: "watching" },
    ]);
    for (const watch of watches) dispatch({ type: "stream", run: transport.run, slot: watch.slot!, stream: new MediaStream() });
    root.render(<TooltipProvider><Stage /></TooltipProvider>);
  });
});

afterEach(async () => {
  await act(async () => { root.unmount(); screen.reset(); });
  container.remove();
  Object.defineProperty(controller, "screen", { value: originalScreen });
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

const video = (share: string) => container.querySelector<HTMLVideoElement>(`video[data-share="${share}"]`)!;
const advance = (ms: number) => act(async () => vi.advanceTimersByTimeAsync(ms));
const focus = (share: string) => act(async () => video(share).closest<HTMLElement>('[role="button"]')!.click());

it("expires both silent grants at 35 s even when the real Stage remounts their videos", async () => {
  const failed = vi.spyOn(screen, "frameFailed");
  const { transport, watches } = useScreen.getState();
  const oldVideos = [video("A"), video("B")];
  const oldHosts = oldVideos.map(v => v.parentElement);
  expect(watches).toHaveLength(2);
  await advance(20_000);
  await focus("A");
  expect(useScreen.getState().stage).toBe("A");
  expect(video("A").parentElement).not.toBe(oldHosts[0]);
  expect(video("B").parentElement).not.toBe(oldHosts[1]);
  expect(video("A")).toBe(oldVideos[0]);
  expect(video("B")).toBe(oldVideos[1]);
  await advance(14_999);
  expect(failed).not.toHaveBeenCalled();
  await advance(1);
  expect(failed.mock.calls).toEqual(watches.map(w => [w.share, transport.run, w.id]));
  expect(useScreen.getState().watches).toEqual([]);
  expect(request.mock.calls.filter(([op]) => op === "screen.unwatch")).toHaveLength(2);
});

it("keeps a static share's first-frame deadline satisfied after Stage remounts", async () => {
  const failed = vi.spyOn(screen, "frameFailed");
  const { transport, watches } = useScreen.getState();
  await advance(10_000);
  await act(async () => video("A").dispatchEvent(new Event("loadeddata")));
  await advance(10_000);
  await focus("A");
  await advance(50_000);
  expect(failed.mock.calls).toEqual([["B", transport.run, watches.find(w => w.share === "B")!.id]]);
  expect(useScreen.getState().watches.map(w => w.share)).toEqual(["A"]);
});

it("moves both attached players with their static frames when the real Stage switches", async () => {
  await act(async () => {
    video("A").dispatchEvent(new Event("loadeddata"));
    video("B").dispatchEvent(new Event("loadeddata"));
  });
  const { transport, watches } = useScreen.getState();
  const oldVideos = [video("A"), video("B")];
  const oldHosts = oldVideos.map(v => v.parentElement);
  request.mockClear();
  await focus("A");
  expect(video("A").parentElement).not.toBe(oldHosts[0]);
  expect(video("B").parentElement).not.toBe(oldHosts[1]);
  expect(video("A")).toBe(oldVideos[0]);
  expect(video("B")).toBe(oldVideos[1]);
  expect(useScreen.getState().transport).toEqual(transport);
  expect(useScreen.getState().watches).toEqual(watches);
  // No extra frame or keyframe request is needed for an already displayed static share.
  await advance(70_000);
  expect(video("A").closest<HTMLElement>("[data-screen-video]")?.dataset.ready).toBe("true");
  expect(video("B").closest<HTMLElement>("[data-screen-video]")?.dataset.ready).toBe("true");
  expect(useScreen.getState().watches).toEqual(watches);
  expect(request.mock.calls.filter(([op]) => op === "screen.keyframe")).toHaveLength(0);
});


it.each(["silent", "receive time", "presentation time", "counter reset"] as const)("keeps a reused slot's readiness and deadline with its new grant (%s)", async frame => {
  const failed = vi.spyOn(screen, "frameFailed");
  const received = vi.spyOn(screen, "frameReceived");
  vi.spyOn(screen, "stats").mockResolvedValue({ width: 640, height: 400, fps: 0, framesDecoded: 1 });
  await act(async () => {
    video("A").dispatchEvent(new Event("loadeddata"));
    video("B").dispatchEvent(new Event("loadeddata"));
  });
  const active = video("A"), old = video("B");
  const run = useScreen.getState().transport.run;
  let frames = frame === "counter reset" ? 22 : 5, callback: VideoFrameRequestCallback | undefined;
  Object.defineProperties(old, {
    readyState: { configurable: true, value: 2 },
    getVideoPlaybackQuality: { configurable: true, value: () => ({ totalVideoFrames: frames }) },
    requestVideoFrameCallback: { configurable: true, value: vi.fn((cb: VideoFrameRequestCallback) => { callback = cb; return 1; }) },
    cancelVideoFrameCallback: { configurable: true, value: vi.fn() },
  });
  await advance(2_000);
  await act(async () => screen.unwatch("B"));
  received.mockClear(); request.mockClear();
  await act(async () => screen.watch("C"));
  const replacement = useScreen.getState().watches.find(w => w.share === "C")!;
  const ready = () => video("C").closest<HTMLElement>("[data-screen-video]")?.dataset.ready;
  expect(video("C")).toBe(old);
  expect(video("A")).toBe(active);
  expect(useScreen.getState().transport.run).toBe(run);
  expect(ready()).toBe("false");
  await act(async () => {
    old.dispatchEvent(new Event("loadeddata"));
    old.dispatchEvent(new Event("resize"));
    if (frame === "counter reset") frames = 1; // WebKit resets playback quality on a player move.
    // A delayed callback for B cannot certify C, even if submitted again later.
    callback?.(performance.now(), { receiveTime: 1469, presentationTime: performance.now() + 1 } as VideoFrameCallbackMetadata);
    // Moving the player and delaying a callback cannot refresh its old frame's timestamp.
    callback?.(performance.now(), { presentationTime: 1469 } as VideoFrameCallbackMetadata);
  });
  await advance(100);
  expect(ready()).toBe("false");
  expect(received.mock.calls.filter(([share]) => share === "C")).toEqual([]);
  expect(request.mock.calls.filter(([op]) => op === "screen.keyframe")).toEqual([["screen.keyframe", { share: "C" }]]);
  await focus("A");
  expect(video("C")).toBe(old);
  await advance(100);
  expect(ready()).toBe("false");
  expect(request.mock.calls.filter(([op]) => op === "screen.keyframe")).toHaveLength(1);
  if (frame !== "silent") {
    // One fresh static frame suffices, also without optional WebRTC metadata.
    frames = frame === "counter reset" ? 2 : frames + 1;
    await act(async () => callback?.(performance.now(), {
      presentationTime: performance.now() + 1,
      ...(frame === "receive time" ? { receiveTime: performance.now() + 1 } : {}),
    } as VideoFrameCallbackMetadata));
    expect(ready()).toBe("true");
    await advance(70_000);
    expect(failed).not.toHaveBeenCalled();
    expect(useScreen.getState().watches.map(w => w.share)).toEqual(["A", "C"]);
  } else {
    await advance(34_799);
    expect(failed).not.toHaveBeenCalled();
    await advance(1);
    expect(failed).toHaveBeenCalledExactlyOnceWith("C", run, replacement.id);
    expect(useScreen.getState().watches.map(w => w.share)).toEqual(["A"]);
  }
  expect(video("A")).toBe(active);
  expect(video("A").closest<HTMLElement>("[data-screen-video]")?.dataset.ready).toBe("true");
});

it("waits out a pause before a silent grant's first frame, however long, and 35 s more after it", async () => {
  const failed = vi.spyOn(screen, "frameFailed");
  const { transport, watches } = useScreen.getState();
  // Join congestion: B pauses before its first frame decodes, and stays paused long after 35 s.
  await advance(11_000);
  await act(async () => { screen.handleEvent({ ev: "screen.paused", d: { share: "B", reason: "bandwidth" } }); });
  await advance(24_000);
  // A still expires on its own clock.
  expect(failed.mock.calls).toEqual([["A", transport.run, watches[0]!.id]]);
  await advance(120_000);
  expect(failed).toHaveBeenCalledTimes(1);
  expect(useScreen.getState().watches.map(w => w.share)).toEqual(["B"]);
  // Once resumed, its first frame has the whole wait again; then a silent share expires.
  await act(async () => { screen.handleEvent({ ev: "screen.resumed", d: { share: "B" } }); });
  await advance(34_999);
  expect(failed).toHaveBeenCalledTimes(1);
  await advance(1);
  expect(failed).toHaveBeenLastCalledWith("B", transport.run, watches[1]!.id);
  expect(useScreen.getState().watches).toEqual([]);
});

it("keeps a share whose first frame arrives after its pause ends", async () => {
  const failed = vi.spyOn(screen, "frameFailed");
  const { transport, watches } = useScreen.getState();
  const b = watches[1]!;
  await advance(11_000);
  await act(async () => { screen.handleEvent({ ev: "screen.paused", d: { share: "B", reason: "bandwidth" } }); });
  await advance(60_000);
  await act(async () => { screen.handleEvent({ ev: "screen.resumed", d: { share: "B" } }); });
  await advance(2_000);
  screen.frameReceived("B", transport.run, b.id);
  await advance(120_000);
  expect(failed.mock.calls.map(([share]) => share)).toEqual(["A"]);
  expect(useScreen.getState().watches.map(w => w.share)).toEqual(["B"]);
});

it("shows why the server holds a share back instead of its last resolution at 0 fps, and retries", async () => {
  // B, watched last, is on the stage; A is a card.
  expect(useScreen.getState().stage).toBe("B");
  await act(async () => { screen.handleEvent({ ev: "screen.paused", d: { share: "B", reason: "bandwidth" } }); });
  const stage = container.querySelector<HTMLElement>('[data-screen-paused="bandwidth"]')!;
  expect(stage.textContent).toContain("Your connection is too slow for this share right now");
  expect(container.querySelector('[data-testid="screen-quality"]')?.textContent).toBe("Paused");
  request.mockClear();
  const retry = [...stage.querySelectorAll("button")].find(b => b.textContent?.includes("Try again"))!;
  await act(async () => retry.click());
  expect(request).toHaveBeenCalledWith("screen.watch", { share: "B" });
  expect(retry.disabled).toBe(true);
  await advance(5000);
  expect(retry.disabled).toBe(false);
  // A card says it briefly; a budget pause offers no retry.
  await act(async () => { screen.handleEvent({ ev: "screen.paused", d: { share: "A", reason: "budget" } }); });
  const card = container.querySelector<HTMLElement>('[data-screen-paused="budget"]')!;
  expect(card.textContent).toBe("Paused");
  await act(async () => {
    screen.handleEvent({ ev: "screen.resumed", d: { share: "B" } });
    screen.handleEvent({ ev: "screen.resumed", d: { share: "A" } });
  });
  expect(container.querySelector("[data-screen-paused]")).toBeNull();
});
