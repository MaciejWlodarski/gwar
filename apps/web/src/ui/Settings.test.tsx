// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { defaultAudio, useSettings } from "../state/settings";
import { useVoice } from "../state/stores";
import { VoiceError } from "../voice/engine";
import { SettingsDialog } from "./Settings";
import { TooltipProvider } from "./kit";

const voice = vi.hoisted(() => ({
  capabilities: { outputDeviceSelection: true, captureProcessing: false as boolean | "noise" },
  listDevices: vi.fn(async () => ({ inputs: [{ id: "replacement", label: "Replacement microphone" }], outputs: [] })),
  startMicTest: vi.fn<(onLevel: (level: number) => void) => Promise<() => void>>(),
}));
vi.mock("../state/controller", () => ({
  controller: { voice },
  describeVoiceError: (error: Error) => error.message,
  UploadError: class extends Error {},
}));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  voice.startMicTest.mockReset();
  useSettings.setState({ language: "en", audio: { ...defaultAudio } });
  useVoice.setState({ state: "idle", level: 0 });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

const button = (label: string) => [...document.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent === label)!;
const meter = () => document.querySelector('[role="meter"]')!;
async function startTest() {
  await act(async () => root.render(<TooltipProvider><SettingsDialog tab="audio" /></TooltipProvider>));
  await act(async () => button("Test microphone").click());
  expect(voice.startMicTest).toHaveBeenCalledTimes(1);
}
async function changeInput() {
  await act(async () => useSettings.getState().setAudio({ inputDeviceId: "replacement" }));
  expect(voice.startMicTest).toHaveBeenCalledTimes(2);
}

describe("microphone test replacement", () => {
  it.each(["superseded", "device unavailable"])("ignores a cancelled startup rejection (%s) after changing input", async (message) => {
    const opening = deferred<() => void>();
    const stopReplacement = vi.fn();
    voice.startMicTest.mockReturnValueOnce(opening.promise).mockResolvedValueOnce(stopReplacement);
    await startTest();
    await changeInput();
    await act(async () => opening.reject(new VoiceError("mic_failed", message)));
    expect(button("Stop test")).toBeDefined();
    expect(document.querySelector('[role="alert"]')).toBeNull();
    expect(stopReplacement).not.toHaveBeenCalled();
    await act(async () => button("Stop test").click());
    expect(stopReplacement).toHaveBeenCalledTimes(1);
    expect(meter().getAttribute("aria-valuenow")).toBe("0");
  });

  it("cleans up a late startup result without letting cancelled levels replace the current meter", async () => {
    const opening = deferred<() => void>();
    const stopReplacement = vi.fn();
    voice.startMicTest.mockReturnValueOnce(opening.promise).mockResolvedValueOnce(stopReplacement);
    await startTest();
    await changeInput();
    const oldLevel = voice.startMicTest.mock.calls[0]![0];
    const currentLevel = voice.startMicTest.mock.calls[1]![0];
    await act(async () => currentLevel(0.6));
    const stopOld = vi.fn(() => oldLevel(0));
    await act(async () => opening.resolve(stopOld));
    expect(stopOld).toHaveBeenCalledTimes(1);
    expect(stopReplacement).not.toHaveBeenCalled();
    await act(async () => oldLevel(0.9));
    expect(meter().getAttribute("aria-valuenow")).toBe("60");
    expect(button("Stop test")).toBeDefined();
    await act(async () => button("Stop test").click());
    expect(meter().getAttribute("aria-valuenow")).toBe("0");
  });

  it("still reports a failure from the current startup", async () => {
    voice.startMicTest.mockRejectedValueOnce(new VoiceError("mic_failed", "device unavailable"));
    await startTest();
    expect(button("Test microphone")).toBeDefined();
    expect(document.querySelector('[role="alert"]')?.textContent).toBe("device unavailable");
  });
});

describe("desktop noise suppression", () => {
  it("offers noise suppression and its strength, and no echo cancellation", async () => {
    voice.capabilities.captureProcessing = "noise";
    try {
      await act(async () => root.render(<TooltipProvider><SettingsDialog tab="audio" /></TooltipProvider>));
      const strength = document.querySelector<HTMLElement>('[role="slider"][aria-label="Noise suppression strength"]')!;
      expect(strength.getAttribute("aria-valuenow")).toBe("100");
      expect([...document.querySelectorAll("*")].some((e) => e.children.length === 0 && e.textContent === "Echo cancellation")).toBe(false);
      await act(async () => strength.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
      expect(useSettings.getState().audio.noiseStrength).toBe(95);
      // Off: no strength to choose.
      await act(async () => useSettings.getState().setAudio({ noiseSuppression: false }));
      expect(document.querySelector('[aria-label="Noise suppression strength"]')).toBeNull();
    } finally {
      voice.capabilities.captureProcessing = false;
    }
  });
});

describe("browser noise suppression", () => {
  it("offers its strength beside echo cancellation", async () => {
    voice.capabilities.captureProcessing = true;
    try {
      await act(async () => root.render(<TooltipProvider><SettingsDialog tab="audio" /></TooltipProvider>));
      const strength = document.querySelector<HTMLElement>('[role="slider"][aria-label="Noise suppression strength"]')!;
      expect(strength.getAttribute("aria-valuenow")).toBe("100");
      expect([...document.querySelectorAll("*")].some((e) => e.children.length === 0 && e.textContent === "Echo cancellation")).toBe(true);
      await act(async () => strength.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true })));
      expect(useSettings.getState().audio.noiseStrength).toBe(95);
      await act(async () => useSettings.getState().setAudio({ noiseSuppression: false }));
      expect(document.querySelector('[aria-label="Noise suppression strength"]')).toBeNull();
    } finally {
      voice.capabilities.captureProcessing = false;
    }
  });
});
