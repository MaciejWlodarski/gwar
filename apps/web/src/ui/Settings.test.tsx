// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { defaultAudio, useSettings } from "../state/settings";
import { useUi, useVoice } from "../state/stores";
import { VoiceError, type MicPermission, type MicTestPlayback } from "../voice/engine";
import { SettingsSection } from "./Settings";
import { TooltipProvider } from "./kit";

const voice = vi.hoisted(() => ({
  capabilities: { outputDeviceSelection: true },
  listDevices: vi.fn(async () => ({ inputs: [{ id: "replacement", label: "Replacement microphone" }], outputs: [] })),
  micPermission: vi.fn<() => Promise<MicPermission>>(),
  startMicTest: vi.fn<(onLevel: (level: number) => void, signal?: AbortSignal, playback?: MicTestPlayback) => Promise<() => void>>(),
}));
vi.mock("../state/controller", () => ({
  controller: { voice }, describeVoiceError: (error: Error) => error.message, UploadError: class extends Error {},
}));
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
let container: HTMLDivElement, root: Root, mounted: boolean, visibility: DocumentVisibilityState;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  visibility = "visible";
  vi.spyOn(document, "visibilityState", "get").mockImplementation(() => visibility);
  voice.startMicTest.mockReset().mockResolvedValue(() => {});
  voice.micPermission.mockReset().mockResolvedValue("granted");
  useSettings.setState({ language: "en", audio: { ...defaultAudio } });
  useVoice.setState({ state: "idle", level: 0, muted: false, deafened: false, inputGateSupport: null });
  useUi.getState().openSettings("audio");
  container = document.createElement("div"); document.body.appendChild(container);
  root = createRoot(container); mounted = true;
});
afterEach(async () => {
  if (mounted) await act(async () => root.unmount());
  container.remove(); vi.restoreAllMocks(); vi.unstubAllGlobals();
});
const button = (text: string) => [...container.querySelectorAll<HTMLButtonElement>("button")].find(b => b.textContent === text)!;
const meter = () => container.querySelector('[role="meter"]')!;
const mount = () => act(async () => root.render(<TooltipProvider><SettingsSection /></TooltipProvider>));
const automatic = () => [...container.querySelectorAll<HTMLElement>('[role="switch"]')].find(e => e.closest("div")?.textContent?.includes("Automatically determine input sensitivity"))!;

it("keeps sensitivity inside Input mode, with a live meter before any mic-test click", async () => {
  await mount();
  expect(voice.startMicTest).toHaveBeenCalledOnce();
  expect(button("Test microphone")).toBeUndefined();
  const headings = [...container.querySelectorAll("h3")].map(h => h.textContent);
  expect(headings.slice(0, 3)).toEqual(["Devices", "Input mode", "Processing"]);
  expect(container.textContent).toContain("Turn off to set it yourself.");
  expect(automatic().getAttribute("aria-checked")).toBe("true");
  await act(async () => voice.startMicTest.mock.calls[0]![0](0.6));
  expect(meter().getAttribute("aria-valuenow")).toBe("60");
  expect(container.querySelectorAll('[role="meter"]')).toHaveLength(1);
});

it("keeps the manual slider keyboard-accessible without visible values or help prose", async () => {
  await mount();
  await act(async () => automatic().click());
  const slider = container.querySelector<HTMLElement>('[role="slider"][aria-label="Input sensitivity"]')!;
  expect(slider.getAttribute("aria-valuetext")).toBe("-50 dB");
  await act(async () => slider.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
  expect(useSettings.getState().audio.vadThreshold).toBe(-51);
  expect(slider.getAttribute("aria-valuetext")).toBe("-51 dB");
  expect(container.textContent).not.toMatch(/dB|Above threshold|Below threshold|Set the marker/);
  expect(voice.startMicTest).toHaveBeenCalledOnce();
});

it.each(["prompt", "denied", "unknown"] as const)("never prompts automatically when permission is %s", async permission => {
  voice.micPermission.mockResolvedValue(permission);
  await mount();
  expect(voice.startMicTest).not.toHaveBeenCalled();
  await act(async () => button("Enable microphone preview").click());
  expect(voice.startMicTest).toHaveBeenCalledOnce();
});

it.each(["starting", "connected", "failed"] as const)("reuses call audio while voice is %s", async state => {
  useVoice.setState({ state, level: 0.6 });
  await mount();
  expect(voice.startMicTest).not.toHaveBeenCalled();
  expect(voice.micPermission).not.toHaveBeenCalled();
  expect(meter().getAttribute("aria-valuenow")).toBe("60");
});

const exits: Array<[string, () => void]> = [
  ["PTT", () => useSettings.getState().setAudio({ inputMode: "ptt" })],
  ["mute", () => useVoice.setState({ muted: true })],
  ["deafen", () => useVoice.setState({ deafened: true })],
  ["voice handover", () => useVoice.setState({ state: "starting" })],
  ["another settings tab", () => useUi.getState().setSettingsTab("appearance")],
  ["another page", () => useUi.getState().setSection("home")],
  ["hidden tab", () => { visibility = "hidden"; document.dispatchEvent(new Event("visibilitychange")); }],
  ["window blur", () => window.dispatchEvent(new Event("blur"))],
];
it.each(exits)("stops local capture immediately on %s", async (_name, leave) => {
  const stop = vi.fn(); voice.startMicTest.mockResolvedValue(stop);
  await mount();
  const signal = voice.startMicTest.mock.calls[0]![1]!;
  await act(async () => {
    leave();
    expect(signal.aborted).toBe(true); // Store/event cancellation is synchronous.
    expect(stop).toHaveBeenCalledOnce();
  });
});

it.each(exits)("cancels recording and playback synchronously on %s", async (_name, leave) => {
  const stop = vi.fn(); voice.startMicTest.mockResolvedValue(stop);
  await mount();
  await act(async () => button("Hear yourself").click());
  const [, signal, playback] = voice.startMicTest.mock.calls.at(-1)!;
  expect(playback).toBeDefined();
  expect(container.textContent).toContain("Speak now");
  await act(async () => playback!.phase("playing"));
  expect(container.textContent).toContain("microphone off");
  await act(async () => {
    leave();
    expect(signal!.aborted).toBe(true);
  });
  await act(async () => playback!.phase("recording"));
  expect(container.textContent).not.toContain("Speak now");
});

it("allows an explicit PTT sample, stops it on request, and never restarts playback on focus", async () => {
  useSettings.getState().setAudio({ inputMode: "ptt" });
  await mount();
  expect(voice.startMicTest).not.toHaveBeenCalled();
  await act(async () => button("Hear yourself").click());
  const signal = voice.startMicTest.mock.calls[0]![1]!;
  await act(async () => button("Stop test").click());
  expect(signal.aborted).toBe(true);
  await act(async () => { window.dispatchEvent(new Event("blur")); window.dispatchEvent(new Event("focus")); });
  expect(voice.startMicTest).toHaveBeenCalledOnce();
});

it("does not restart a sample when focus returns and ignores a late failure", async () => {
  await mount();
  await act(async () => button("Hear yourself").click());
  const playback = voice.startMicTest.mock.calls.at(-1)![2]!;
  await act(async () => { window.dispatchEvent(new Event("blur")); window.dispatchEvent(new Event("focus")); });
  expect(voice.startMicTest.mock.calls.at(-1)![2]).toBeUndefined();
  await act(async () => playback.error(new Error("obsolete")));
  expect(container.querySelector('[role="alert"]')).toBeNull();
});

it("does not capture when the page opens in an unfocused window", async () => {
  const focus = vi.spyOn(document, "hasFocus").mockReturnValue(false);
  await mount();
  expect(voice.startMicTest).not.toHaveBeenCalled();
  focus.mockReturnValue(true);
  await act(async () => window.dispatchEvent(new Event("focus")));
  expect(voice.startMicTest).toHaveBeenCalledOnce();
});

it("stays closed when the tab returns without focus", async () => {
  await mount();
  await act(async () => window.dispatchEvent(new Event("blur")));
  await act(async () => { visibility = "hidden"; document.dispatchEvent(new Event("visibilitychange")); });
  await act(async () => { visibility = "visible"; document.dispatchEvent(new Event("visibilitychange")); });
  expect(voice.startMicTest).toHaveBeenCalledOnce();
  await act(async () => window.dispatchEvent(new Event("focus")));
  expect(voice.startMicTest).toHaveBeenCalledTimes(2);
});

it("stops active capture immediately when the Audio page unmounts", async () => {
  const stop = vi.fn(); voice.startMicTest.mockResolvedValue(stop);
  await mount();
  await act(async () => { root.unmount(); mounted = false; });
  expect(stop).toHaveBeenCalledOnce();
  expect(voice.startMicTest.mock.calls[0]![1]!.aborted).toBe(true);
});

it("stops on unmount and does not reopen from a late permission result", async () => {
  const permission = deferred<MicPermission>(); voice.micPermission.mockReturnValue(permission.promise);
  await mount();
  await act(async () => { root.unmount(); mounted = false; });
  await act(async () => permission.resolve("granted"));
  expect(voice.startMicTest).not.toHaveBeenCalled();
});

it.each(exits)("retires a pending capture on %s and closes its late result", async (_name, leave) => {
  const opening = deferred<() => void>(); voice.startMicTest.mockReturnValue(opening.promise);
  await mount();
  const signal = voice.startMicTest.mock.calls[0]![1]!;
  await act(async () => leave());
  expect(signal.aborted).toBe(true);
  const stop = vi.fn();
  await act(async () => opening.resolve(stop));
  expect(stop).toHaveBeenCalledOnce();
});

it("ignores obsolete levels and startup failures after a device change", async () => {
  const opening = deferred<() => void>(); voice.startMicTest.mockReturnValueOnce(opening.promise).mockResolvedValue(() => {});
  await mount();
  const oldLevel = voice.startMicTest.mock.calls[0]![0];
  await act(async () => useSettings.getState().setAudio({ inputDeviceId: "replacement" }));
  expect(voice.startMicTest).toHaveBeenCalledTimes(2);
  await act(async () => voice.startMicTest.mock.calls[1]![0](0.6));
  await act(async () => { opening.reject(new VoiceError("mic_failed", "obsolete")); oldLevel(0.9); });
  expect(meter().getAttribute("aria-valuenow")).toBe("60");
  expect(container.querySelector('[role="alert"]')).toBeNull();
});

it("hides the sensitivity section in PTT without opening capture", async () => {
  useSettings.getState().setAudio({ inputMode: "ptt" });
  await mount();
  expect([...container.querySelectorAll("h3")].some(h => h.textContent === "Input sensitivity")).toBe(false);
  expect(voice.startMicTest).not.toHaveBeenCalled();
});

it("keeps noise-suppression strength beside echo cancellation", async () => {
  await mount();
  const strength = container.querySelector<HTMLElement>('[role="slider"][aria-label="Noise suppression strength"]')!;
  await act(async () => strength.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
  expect(useSettings.getState().audio.noiseStrength).toBe(95);
});

it("explains unsupported manual sensitivity while preserving the saved choice", async () => {
  useSettings.getState().setAudio({ automaticSensitivity: false, vadThreshold: -80 });
  useVoice.setState({ inputGateSupport: false });
  await mount();
  expect(automatic().getAttribute("aria-checked")).toBe("true");
  expect(automatic().hasAttribute("disabled")).toBe(true);
  expect(container.textContent).toContain("Manual sensitivity needs a newer server.");
  expect(container.querySelector('[role="slider"][aria-label="Input sensitivity"]')).toBeNull();
  expect(useSettings.getState().audio).toMatchObject({ automaticSensitivity: false, vadThreshold: -80 });
});

it.each([
  { inputDeviceId: "replacement" }, { outputDeviceId: "replacement" },
  { noiseStrength: 50 }, { masterVolume: 0 },
])("retires a sample before changing devices or processing: %j", async patch => {
  await mount();
  await act(async () => button("Hear yourself").click());
  const signal = voice.startMicTest.mock.calls.at(-1)![1]!;
  await act(async () => {
    useSettings.getState().setAudio(patch);
    expect(signal.aborted).toBe(true);
  });
  expect(voice.startMicTest.mock.calls.at(-1)![2]).toBeUndefined();
});
