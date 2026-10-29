import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initialScreen, useScreen } from "./screen";
import { canWatch, ScreenController, screenSupport, useDesktopCapture } from "./screen-controller";
import { useUi } from "./stores";
import { legacySpec } from "../voice/screen-quality";
import { notify } from "../platform/notify";
vi.mock("../platform/notify", () => ({ notify: vi.fn() }));

const shell = vi.hoisted(() => ({ invoke: vi.fn<(command: string) => Promise<unknown>>() }));
vi.mock("../platform", () => ({
  isDesktop: () => true,
  tauri: async () => ({ invoke: shell.invoke, listen: async () => () => {} }),
}));

const getDisplayMedia = vi.fn(() => new Promise<MediaStream>(() => {}));
const flush = () => new Promise(resolve => setTimeout(resolve, 0));
const toast = vi.fn();

beforeEach(() => {
  shell.invoke.mockReset();
  vi.mocked(notify).mockClear();
  getDisplayMedia.mockClear();
  useDesktopCapture.setState({ capture: null });
  useScreen.setState({ ...initialScreen });
  toast.mockReset();
  useUi.setState({ toast });
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

  it("asks the shell once at start: macOS 14.6+ and Windows share and watch", async () => {
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

  it("macOS before 14.6 watches but never opens the picker", async () => {
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

  it("cancelling a picker stays silent, however long it took", async () => {
    shell.invoke.mockResolvedValue("ok");
    const screen = new ScreenController(() => null, String);
    screen.init();
    await flush();
    // A refusal before any picker and the user closing it raise the same error; timing tells nothing.
    for (let i = 0; i < 2; i++) {
      toast.mockReset();
      getDisplayMedia.mockImplementationOnce(async () => { throw new DOMException("The request is not allowed", "NotAllowedError"); });
      screen.beginShare();
      await flush();
      expect(toast).not.toHaveBeenCalled();
      expect(useScreen.getState().notice).toBeNull();
      expect(useScreen.getState().own).toBeNull();
    }
  });
  it("OS denial has persistent guidance and no browser error toast", async () => {
    useDesktopCapture.setState({ capture: "ok" });
    getDisplayMedia.mockImplementationOnce(async () => { throw new DOMException("Permission denied by system", "NotAllowedError"); });
    new ScreenController(() => null, String).beginShare();
    await flush();
    expect(useScreen.getState().notice).toBe("permission");
    expect(toast).not.toHaveBeenCalled();
  });

  it("also notifies the desktop sharer when retained capture ends", () => {
    const track = { readyState: "ended", stop: vi.fn() };
    useScreen.setState({ own: { phase: "live", attempt: 1, share: "M", stream: { getVideoTracks: () => [track], getTracks: () => [track] } as unknown as MediaStream,
      surface: "window", spec: legacySpec("text"), viewers: 0, guests: 0, guestLink: false, preview: false } });
    const screen = new ScreenController(() => null, String);
    screen.suspend();
    expect(useScreen.getState().notice).toBe("reconnect");
    expect(notify).toHaveBeenCalledExactlyOnceWith({ title: "Gwar", body: "Your screen share stopped after the connection was lost — share again.", tag: "screen-reconnect" });
    expect(track.stop).toHaveBeenCalledOnce();
    screen.reset();
  });

});
