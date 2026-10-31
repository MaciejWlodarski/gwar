// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { initialState } from "../state/reducer";
import { initialScreen, useScreen } from "../state/screen";
import { useSession } from "../state/stores";
import { useSettings } from "../state/settings";
import { controller } from "../state/controller";
import { ShareAudioPlayback, ShareAudioControls, SharePresetDialog, SharingBar } from "./Screen";
import { TooltipProvider } from "./kit";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
it("has one output per other share across focus changes, with independent mute, volume and deafen", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
  const stream = new MediaStream([{ kind: "audio" } as MediaStreamTrack]);
  useSettings.setState({ language: "en", shareAudio: { A: { volume: 0.35, muted: true }, B: { volume: 0.8, muted: false } }, selfDeafened: false });
  useSession.setState({ ...initialState, me: { session: 1, uid: "me" }, clients: { 2: { uid: "friend" } } as never,
    shares: { A: { share: "A", client: 2 }, B: { share: "B", client: 2 }, SELF: { share: "SELF", client: 1 } } as never });
  useScreen.setState({ ...initialScreen, stage: "A", streams: [stream, stream, stream, null], watches: [
    { share: "A", id: 1, slot: 0, status: "watching", audio: true }, { share: "B", id: 2, slot: 1, status: "watching", audio: true }, { share: "SELF", id: 3, slot: 2, status: "watching", audio: true },
  ] });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<TooltipProvider><ShareAudioPlayback /><ShareAudioControls share="A" /></TooltipProvider>));
    const audio = host.querySelector("audio")!;
    expect(host.querySelectorAll("audio")).toHaveLength(2);
    expect(audio.dataset.shareAudio).toBe("A");
    const second = host.querySelector<HTMLAudioElement>('audio[data-share-audio="B"]')!;
    expect(second.volume).toBe(0.8); expect(second.muted).toBe(false);
    expect(host.querySelector('audio[data-share-audio="SELF"]')).toBeNull();
    expect(audio.muted).toBe(true); expect(audio.volume).toBe(0.35);
    await act(async () => useSettings.getState().setShareAudio("A", { muted: false }));
    expect(audio.muted).toBe(false); expect(audio.volume).toBe(0.35);
    await act(async () => useScreen.setState({ stage: "SELF" }));
    expect(host.querySelector("audio")).toBe(audio);
    for (const action of [{ type: "paused", share: "A", reason: "bandwidth" }, { type: "waiting", share: "A" }, { type: "resumed", share: "A" }] as const) {
      await act(async () => useScreen.getState().dispatch(action));
      expect(host.querySelector('audio[data-share-audio="A"]')).toBe(audio);
      expect(audio.muted).toBe(false);
      expect(host.querySelector('[data-testid="share-audio-controls"]')).not.toBeNull();
    }
    await act(async () => useSettings.setState({ selfDeafened: true }));
    expect(audio.muted).toBe(true);
    await act(async () => useScreen.setState({ watches: [] }));
    expect(host.querySelector("audio")).toBeNull(); expect(audio.srcObject).toBeNull();
  } finally { await act(async () => root.unmount()); host.remove(); }
});

it.each(["desktop", "video-only"])("never plays or offers sound for a %s viewer", async kind => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockRejectedValue(new Error("autoplay"));
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
  if (kind === "desktop") Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  useSettings.setState({ language: "en" });
  useSession.setState({ ...initialState, me: { session: 1, uid: "me" }, clients: { 2: { uid: "friend" } } as never, shares: { A: { share: "A", client: 2 } } as never });
  useScreen.setState({ ...initialScreen, streams: [new MediaStream([{ kind: "audio" } as MediaStreamTrack]), null, null, null],
    slots: ["A", null, null, null], watches: [{ share: "A", id: 1, slot: 0, status: "watching", audio: kind === "desktop" }] });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<TooltipProvider><ShareAudioPlayback /><ShareAudioControls share="A" /></TooltipProvider>));
    expect(play).not.toHaveBeenCalled();
    expect(host.querySelector("audio, input[type=range]")).toBeNull();
    expect(host.textContent).not.toContain("Play share sound");
  } finally { await act(async () => root.unmount()); host.remove(); delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__; }
});

function desktop(): () => void {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  return () => { delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__; };
}

it("plays through the native mixer on the desktop app, offering controls only once it really can", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const undo = desktop();
  const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
  const open = vi.spyOn(controller.screen, "openSound").mockResolvedValue();
  const close = vi.spyOn(controller.screen, "closeSound").mockImplementation(() => {});
  const gain = vi.spyOn(controller.screen, "setSoundGain").mockImplementation(() => {});
  useSettings.setState({ language: "en", shareAudio: { A: { volume: 0.5, muted: false } } });
  useSession.setState({ ...initialState, me: { session: 1, uid: "me" }, clients: { 1: { uid: "me" }, 2: { uid: "friend" } } as never, shares: { A: { share: "A", client: 2 }, SELF: { share: "SELF", client: 1 } } as never });
  useScreen.setState({ ...initialScreen, transport: { phase: "connected", run: 4 }, slots: ["A", "SELF", null, null], streams: [new MediaStream(), new MediaStream(), null, null], watches: [{ share: "A", id: 1, slot: 0, status: "watching", audio: true }, { share: "SELF", id: 2, slot: 1, status: "watching", audio: true }] });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<TooltipProvider><ShareAudioPlayback /><ShareAudioControls share="A" /></TooltipProvider>));
    expect(open.mock.calls).toEqual([[1, "A", 0]]);
    expect(host.querySelector("audio")).toBeNull();
    expect(play).not.toHaveBeenCalled();
    expect(host.querySelector('[data-testid="share-audio-controls"]')).toBeNull();
    await act(async () => useScreen.getState().dispatch({ type: "watchSound", id: 1, ready: true }));
    expect(host.querySelector('[data-testid="share-audio-controls"]')).not.toBeNull();
    gain.mockClear();
    await act(async () => useSettings.getState().setShareAudio("A", { muted: true }));
    expect(gain).toHaveBeenCalledWith("A", 0);
    await act(async () => useScreen.setState({ watches: [] }));
    expect(close).toHaveBeenCalledWith(1, 0);
  } finally { await act(async () => root.unmount()); host.remove(); undo(); }
});

const ownShare = (extra: object) => ({ phase: "choosing" as const, attempt: 1, share: null, stream: null, surface: "screen" as const, spec: { mode: "smooth" as const, height: 720 as const, fps: 30 as const }, viewers: 0, guests: 0, guestLink: false, preview: false, ...extra });

it.each([
  ["ok", "Share computer sound"],
  ["permission", "needs Screen Recording"],
  ["unsupported", "needs macOS 14"],
] as const)("asks about computer sound honestly on the desktop app: %s", async (computerSound, text) => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const undo = desktop();
  useSettings.setState({ language: "en", shareComputerSound: false });
  useSession.setState({ ...initialState });
  useScreen.setState({ ...initialScreen, own: ownShare({ computerSound }) });
  const root = createRoot(document.createElement("div"));
  try {
    await act(async () => root.render(<TooltipProvider><SharePresetDialog /></TooltipProvider>));
    const choice = document.querySelector('[data-testid="computer-sound-choice"]')!;
    expect(choice.textContent).toContain(text);
    const box = choice.querySelector<HTMLInputElement>("input[type=checkbox]");
    if (computerSound !== "ok") { expect(box).toBeNull(); return; }
    expect(box!.checked).toBe(false);
    await act(async () => box!.click());
    expect(useSettings.getState().shareComputerSound).toBe(true);
  } finally { await act(async () => root.unmount()); undo(); }
});

it("says why there is no computer sound in Details, and can take sharing back", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  const undo = desktop();
  const stop = vi.spyOn(controller.screen, "stopComputerSound").mockImplementation(() => {});
  useSettings.setState({ language: "en" });
  useScreen.setState({ ...initialScreen, own: ownShare({ phase: "live", share: "M", audio: true, computerSound: "ok", viewers: 1 }) });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  const details = () => [...host.querySelectorAll("button")].find(b => b.textContent === "Details")!;
  try {
    await act(async () => root.render(<TooltipProvider><SharingBar /></TooltipProvider>));
    await act(async () => details().click());
    expect(document.body.textContent).toContain("Computer sound is shared, except sounds from Gwar.");
    await act(async () => [...document.querySelectorAll("button")].find(b => b.textContent === "Stop sharing sound")!.click());
    expect(stop).toHaveBeenCalled();
    await act(async () => useScreen.setState({ own: ownShare({ phase: "live", share: "M", audio: false, computerSound: "stopped", viewers: 1 }) }));
    expect(document.body.textContent).toContain("Computer sound is off for this share.");
    expect([...document.querySelectorAll("button")].some(b => b.textContent === "Stop sharing sound")).toBe(false);
  } finally { await act(async () => root.unmount()); host.remove(); undo(); }
});

it.each([[0, false], [0, true], [2, true]] as const)("keeps %s watchers in the sharing status beside audio=%s", async (viewers, audio) => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  useScreen.setState({ ...initialScreen, own: { phase: "live", attempt: 1, share: "M", stream: null, surface: "tab", spec: { mode: "smooth", height: 720, fps: 30 }, viewers, guests: 0, guestLink: false, preview: false, audio } });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<TooltipProvider><SharingBar /></TooltipProvider>));
    const status = host.querySelector('[role="status"]')!;
    expect(status.querySelector('[data-testid="screen-viewers"]')?.textContent).toBe(`${viewers} watching`);
    expect(status.querySelector('[data-testid="share-audio-status"]')).not.toBeNull();
  } finally { await act(async () => root.unmount()); host.remove(); }
});

it("shows the live guest link count and turn-off action without opening details", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  useScreen.setState({ ...initialScreen, links: { M: true }, own: { phase: "live", attempt: 1, share: "M", stream: null, surface: "tab", spec: { mode: "smooth", height: 720, fps: 30 }, viewers: 0, guests: 2, guestLink: true, preview: false, audio: true } });
  const host = document.createElement("div"); document.body.append(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<TooltipProvider><SharingBar /></TooltipProvider>));
    const link = host.querySelector('[data-testid="screen-ts-link"]');
    expect(link?.textContent).toContain("TeamSpeak link on · 2 guests");
    expect(link?.querySelector("button")?.textContent).toBe("Turn off");
    expect(host.querySelector('[data-testid="screen-viewers"]')?.textContent).toBe("2 watching");
    expect(host.querySelector('[data-testid="share-audio-status"]')?.getAttribute("aria-label")).toBe("Sound on");
  } finally { await act(async () => root.unmount()); host.remove(); }
});
