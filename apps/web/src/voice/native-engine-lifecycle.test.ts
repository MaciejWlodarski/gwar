import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { VoiceEngine } from "./engine";
import { NativeVoiceEngine } from "./native-engine";

const bridge = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("../platform", () => ({ tauri: async () => bridge }));

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return { promise, resolve };
}

const params = (offer = vi.fn(async () => "answer")): Parameters<VoiceEngine["start"]>[0] => ({
  offer, mode: "plaintext", iceServers: [], serverHost: "127.0.0.1",
  binding: { server_key: null, session: 1, channel: 1, call: null, participation: null, group: null },
});

let owner: number;
let activeRun: number;
let cancelledThrough: number;
let bootstrapReply: Promise<void> | null;
let listeners: Map<string, Set<(payload: never) => void>>;
let engines: NativeVoiceEngine[];
const engine = () => {
  const value = new NativeVoiceEngine();
  engines.push(value);
  return value;
};
function emit(event: string, payload: unknown) {
  for (const listener of listeners.get(event) ?? []) listener(payload as never);
}

beforeEach(() => {
  vi.resetAllMocks();
  vi.spyOn(console, "warn").mockImplementation(() => {});
  owner = 0;
  // Simulate native state retained across a web-view reload.
  activeRun = 1_800_000_000_000_123;
  cancelledThrough = 0;
  bootstrapReply = null;
  listeners = new Map();
  engines = [];
  bridge.listen.mockImplementation(async (event: string, listener: (payload: never) => void) => {
    const handlers = listeners.get(event) ?? new Set();
    listeners.set(event, handlers);
    handlers.add(listener);
    return () => handlers.delete(listener);
  });
  bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === "voice_bootstrap") {
      const claimed = ++owner;
      activeRun = 0;
      cancelledThrough = 0;
      await bootstrapReply;
      return claimed;
    }
    if (command === "audio_devices") return { inputs: [], outputs: [] };
    if (args?.owner !== owner) throw new Error("superseded owner");
    if (command === "voice_start") {
      const run = args.run as number;
      if (run <= cancelledThrough) throw new Error("superseded");
      activeRun = run;
    } else if (command === "voice_stop") {
      const run = args.run as number;
      cancelledThrough = Math.max(cancelledThrough, run);
      if (activeRun <= run) activeRun = 0;
    }
  });
});

afterEach(async () => {
  await Promise.all(engines.map((value) => value.stop()));
  vi.restoreAllMocks();
});

describe("native web-view ownership", () => {
  it("bootstraps on construction and retires a previous timestamp-sized run before the first stop", async () => {
    const view = engine();
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("voice_bootstrap"));
    expect(activeRun).toBe(0);
    await view.stop();
    expect(bridge.invoke).toHaveBeenCalledWith("voice_stop", { owner: 1, run: 0 });
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "voice_bootstrap")).toHaveLength(1);
  });

  it("waits for the bootstrap token before starting, controlling or testing capture", async () => {
    const bootstrap = deferred<void>();
    bootstrapReply = bootstrap.promise;
    const view = engine();
    view.setMuted(true);
    view.setPttActive(true);
    const starting = view.start(params());
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledTimes(1));
    expect(bridge.invoke.mock.calls[0]).toEqual(["voice_bootstrap"]);
    bootstrap.resolve();
    await starting;
    const stopTest = await view.startMicTest(vi.fn());
    stopTest();
    await view.stop();
    expect(bridge.invoke).toHaveBeenCalledWith("voice_start", expect.objectContaining({ owner: 1 }));
    expect(bridge.invoke).toHaveBeenCalledWith("mic_test_start", { owner: 1, input: null });
    expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1 });
    expect(bridge.invoke.mock.calls.filter(([command]) => command !== "voice_bootstrap" && command !== "audio_devices").every(([, args]) => args?.owner === 1)).toBe(true);
  });

  it("keeps the old token on delayed answers and controls after a new view bootstraps", async () => {
    const answer = deferred<string>();
    const old = engine();
    await old.start(params(vi.fn(() => answer.promise)));
    const oldRun = activeRun;
    emit("voice://offer", { owner: 1, run: oldRun, sdp: "offer" });
    const current = engine();
    await current.start(params());
    const currentRun = activeRun;
    answer.resolve("late answer");
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("voice_answer", { owner: 1, run: oldRun, sdp: "late answer" }));
    old.setPttActive(true);
    await old.stop();
    expect(activeRun).toBe(currentRun);
    expect(bridge.invoke).toHaveBeenCalledWith("voice_set_ptt", { owner: 1, active: true });
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "voice_bootstrap")).toHaveLength(2);
  });

  it("ignores delayed events from the previous owner even when run numbers match", async () => {
    const view = engine();
    const state = vi.fn();
    const offer = vi.fn(async () => "answer");
    view.on("state", state);
    await view.start(params(offer));
    state.mockClear();
    emit("voice://state", { owner: 0, run: activeRun, state: "connected" });
    emit("voice://offer", { owner: 0, run: activeRun, sdp: "obsolete" });
    expect(state).not.toHaveBeenCalled();
    expect(offer).not.toHaveBeenCalled();
    emit("voice://state", { owner: 1, run: activeRun, state: "connected" });
    expect(state).toHaveBeenCalledWith("connected");
  });
});

describe("native microphone test replacement", () => {
  it("keeps late startup cleanup and levels from stopping or updating the replacement", async () => {
    const opening = deferred<void>();
    const entered = deferred<void>();
    const invoke = bridge.invoke.getMockImplementation()!;
    let first = true;
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "mic_test_start" && first) {
        first = false;
        entered.resolve();
        await opening.promise;
      }
      return invoke(command, args);
    });
    const view = engine();
    const oldLevel = vi.fn();
    const currentLevel = vi.fn();
    const oldTest = view.startMicTest(oldLevel);
    await entered.promise;
    const stopCurrent = await view.startMicTest(currentLevel);
    emit("voice://test-level", { owner: 1, db: -30 });
    expect(oldLevel).not.toHaveBeenCalled();
    expect(currentLevel).toHaveBeenCalledWith(0.5);
    opening.resolve();
    const stopOld = await oldTest;
    stopOld();
    await view.stop(); // Drain queued commands before asserting no stale mic stop was sent.
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_stop")).toHaveLength(0);
    emit("voice://test-level", { owner: 1, db: -30 });
    expect(oldLevel).not.toHaveBeenCalled();
    expect(currentLevel).toHaveBeenCalledWith(0.5);
    stopCurrent();
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1 }));
  });

  it("does not start an obsolete test whose event listener setup finishes after the replacement", async () => {
    const listening = deferred<void>();
    const entered = deferred<void>();
    const listen = bridge.listen.getMockImplementation()!;
    let first = true;
    bridge.listen.mockImplementation(async (event: string, handler: (payload: never) => void) => {
      if (event === "voice://test-level" && first) {
        first = false;
        entered.resolve();
        await listening.promise;
      }
      return listen(event, handler);
    });
    const view = engine();
    const oldTest = view.startMicTest(vi.fn());
    await entered.promise;
    const stopCurrent = await view.startMicTest(vi.fn());
    listening.resolve();
    const stopOld = await oldTest;
    stopOld();
    await view.stop(); // Drain queued commands before asserting no stale mic stop was sent.
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_start")).toHaveLength(1);
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_stop")).toHaveLength(0);
    stopCurrent();
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1 }));
  });
});
