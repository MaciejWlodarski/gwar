import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initialScreen, useScreen } from "./screen";
import { canWatch, ScreenController, screenSupport, useDesktopCapture } from "./screen-controller";

const shell = vi.hoisted(() => ({ invoke: vi.fn<(command: string) => Promise<unknown>>() }));
vi.mock("../platform", () => ({
  isDesktop: () => true,
  tauri: async () => ({ invoke: shell.invoke, listen: async () => () => {} }),
}));

const getDisplayMedia = vi.fn(() => new Promise<MediaStream>(() => {}));
const flush = () => new Promise(resolve => setTimeout(resolve, 0));

beforeEach(() => {
  shell.invoke.mockReset();
  getDisplayMedia.mockClear();
  useDesktopCapture.setState({ capture: null });
  useScreen.setState({ ...initialScreen });
  vi.stubGlobal("RTCPeerConnection", class {});
  vi.stubGlobal("window", { isSecureContext: true });
  vi.stubGlobal("navigator", { mediaDevices: { getDisplayMedia } });
});
afterEach(() => { vi.unstubAllGlobals(); });

describe("the desktop app", () => {
  it("neither shares nor watches until the shell answers", () => {
    expect(screenSupport()).toBe("hidden");
    expect(canWatch()).toBe(false);
    new ScreenController(() => null, String).beginShare();
    expect(getDisplayMedia).not.toHaveBeenCalled();
  });

  it("asks the shell once at start: macOS 14+ and Windows share and watch", async () => {
    shell.invoke.mockResolvedValue("ok");
    const screen = new ScreenController(() => null, String);
    screen.init();
    screen.init();
    await flush();
    expect(shell.invoke.mock.calls).toEqual([["screen_capture_support"]]);
    expect(screenSupport()).toBe("ok");
    expect(canWatch()).toBe(true);
    screen.beginShare();
    expect(getDisplayMedia).toHaveBeenCalledTimes(1);
  });

  it("macOS before 14 watches but never opens the picker", async () => {
    shell.invoke.mockResolvedValue("macos_too_old");
    const screen = new ScreenController(() => null, String);
    screen.init();
    await flush();
    expect(screenSupport()).toBe("macos");
    expect(canWatch()).toBe(true);
    screen.beginShare();
    expect(getDisplayMedia).not.toHaveBeenCalled();
    expect(useScreen.getState().own).toBeNull();
  });

  it("Linux, or a shell that cannot answer, neither shares nor watches", async () => {
    for (const answer of [async () => "unsupported", async () => { throw new Error("no such command"); }]) {
      useDesktopCapture.setState({ capture: null });
      shell.invoke.mockImplementationOnce(answer);
      vi.spyOn(console, "warn").mockImplementation(() => {});
      new ScreenController(() => null, String).init();
      await flush();
      expect(screenSupport()).toBe("hidden");
      expect(canWatch()).toBe(false);
    }
  });
});
