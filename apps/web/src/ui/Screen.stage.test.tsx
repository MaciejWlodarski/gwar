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
    welcome: welcome({ shares: ["A", "B"].map(share => ({ share, client: 2, channel: 1, preset: "text", surface: "screen" })) }),
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
