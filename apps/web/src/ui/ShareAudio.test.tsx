// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { initialState } from "../state/reducer";
import { initialScreen, useScreen } from "../state/screen";
import { useSession } from "../state/stores";
import { useSettings } from "../state/settings";
import { ShareAudioPlayback, ShareAudioControls } from "./Screen";
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
    { share: "A", id: 1, slot: 0, status: "watching" }, { share: "B", id: 2, slot: 1, status: "watching" }, { share: "SELF", id: 3, slot: 2, status: "watching" },
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
    await act(async () => useSettings.setState({ selfDeafened: true }));
    expect(audio.muted).toBe(true);
    await act(async () => useScreen.setState({ watches: [] }));
    expect(host.querySelector("audio")).toBeNull(); expect(audio.srcObject).toBeNull();
  } finally { await act(async () => root.unmount()); host.remove(); }
});
