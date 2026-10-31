import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { TauriBridge } from ".";

vi.mock("../state/settings", async () => {
  const { create } = await import("zustand");
  return { useSettings: create(() => ({ audio: { inputMode: "ptt", pttKey: "AltRight" } })) };
});
vi.mock("../state/stores", async () => {
  const { create } = await import("zustand");
  return { useVoice: create(set => ({ pttActive: false, set })) };
});
vi.mock(".", () => ({ isDesktop: () => true }));
import { useSettings } from "../state/settings";
import { useVoice } from "../state/stores";
import { initGlobalPtt, mousePttCode, nativeOwnsPtt, useGlobalPtt } from "./ptt";

const flush = async () => { for (let i = 0; i < 20; i++) await Promise.resolve(); };
let cleanup: (() => void) | undefined;
beforeEach(() => {
  vi.useFakeTimers();
  useSettings.setState(s => ({ audio: { ...s.audio, inputMode: "ptt", pttKey: "AltRight" } }));
  useGlobalPtt.setState({ status: "off", native: true, capturing: false });
});
afterEach(async () => { cleanup?.(); cleanup = undefined; await flush(); vi.useRealTimers(); });

function bridge(status: string) {
  const handlers = new Map<string, (value: never) => void>();
  const invoke = vi.fn(async (command: string) => command === "set_global_ptt" ? status : "recovering");
  const api = { invoke, listen: vi.fn(async (event: string, handler: (value: never) => void) => {
    handlers.set(event, handler); return () => { handlers.delete(event); };
  }) } as unknown as TauriBridge;
  return { api, invoke, emit: (event: string, payload: unknown) => handlers.get(event)?.(payload as never) };
}

describe("passive PTT wiring", () => {
  it.each(["permission", "recovering", "unsupported_key"])("%s cannot fall through to window capture or a consuming shortcut", async status => {
    const b = bridge(status);
    cleanup = await initGlobalPtt(b.api); await flush();
    expect(b.invoke).toHaveBeenCalledWith("set_global_ptt", { key: "AltRight" });
    expect(nativeOwnsPtt()).toBe(true);
    expect(useGlobalPtt.getState().status).toBe(status);
    expect(b.invoke).not.toHaveBeenCalledWith("set_global_shortcut", expect.anything());
    b.emit("ptt://state", true);
    expect(useVoice.getState().pttActive).toBe(false);
    expect(b.invoke).not.toHaveBeenCalledWith("voice_set_ptt", expect.anything());
  });

  it("detects permission grants and failed supervision without reopening capture from the UI", async () => {
    const b = bridge("permission");
    cleanup = await initGlobalPtt(b.api); await flush();
    b.emit("ptt://status", "ready");
    expect(useGlobalPtt.getState().status).toBe("ready");
    await vi.advanceTimersByTimeAsync(1000);
    expect(useGlobalPtt.getState().status).toBe("recovering");
    expect(nativeOwnsPtt()).toBe(true);
  });

  it("suspends global PTT while recording, persists a side button, and closes on cleanup", async () => {
    const b = bridge("ready");
    cleanup = await initGlobalPtt(b.api); await flush();
    useGlobalPtt.setState({ capturing: true }); await flush();
    expect(b.invoke).toHaveBeenLastCalledWith("set_global_ptt", { key: null });
    useSettings.setState(s => ({ audio: { ...s.audio, pttKey: "Mouse4" } }));
    useGlobalPtt.setState({ capturing: false }); await flush();
    expect(b.invoke).toHaveBeenLastCalledWith("set_global_ptt", { key: "Mouse4" });
    cleanup(); cleanup = undefined; await flush();
    expect(b.invoke).toHaveBeenLastCalledWith("set_global_ptt", { key: null });
    expect(useVoice.getState().pttActive).toBe(false);
  });

  it("a permission-grant event wins over an older in-flight status reply", async () => {
    const b = bridge("ready");
    let resolve!: (s: string) => void;
    b.invoke.mockImplementationOnce(() => new Promise<string>(r => { resolve = r; }));
    cleanup = await initGlobalPtt(b.api); await flush();
    b.emit("ptt://status", "ready");
    b.emit("ptt://state", true);
    resolve("permission"); await flush();
    expect(useGlobalPtt.getState().status).toBe("ready");
    expect(useVoice.getState().pttActive).toBe(true);
    expect(b.invoke).not.toHaveBeenCalledWith("voice_set_ptt", expect.anything());
  });

  it("does not let an old asynchronous registration replace the latest selection", async () => {
    const b = bridge("ready");
    let resolve!: (s: string) => void;
    b.invoke.mockImplementationOnce(() => new Promise<string>(r => { resolve = r; }));
    cleanup = await initGlobalPtt(b.api); await flush();
    useSettings.setState(s => ({ audio: { ...s.audio, pttKey: "Mouse5" } }));
    resolve("permission"); await flush();
    expect(useGlobalPtt.getState().status).toBe("ready");
    expect(b.invoke).toHaveBeenLastCalledWith("set_global_ptt", { key: "Mouse5" });
  });

  it("keeps the existing Linux shortcut where available and a focused-window fallback otherwise", async () => {
    const b = bridge("local_only");
    cleanup = await initGlobalPtt(b.api); await flush();
    expect(useGlobalPtt.getState()).toMatchObject({ status: "legacy", native: false });
    cleanup(); cleanup = undefined; await flush();
    b.invoke.mockImplementation(async command => { if (command === "set_global_shortcut") throw new Error("unavailable"); return "local_only"; });
    cleanup = await initGlobalPtt(b.api); await flush();
    expect(useGlobalPtt.getState()).toMatchObject({ status: "local_only", native: false });
  });

  it("maps middle and side buttons without binding a normal click", () => {
    expect([0, 1, 2, 3, 4, 5].map(mousePttCode)).toEqual([null, "Mouse3", null, "Mouse4", "Mouse5", null]);
  });
});
