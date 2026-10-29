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
    if (command === "mic_permission") return "granted";
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
  it("applies manual sensitivity live, preserves it across PTT, and restores the automatic default", async () => {
    const view = engine();
    view.setInputSensitivity(-72);
    view.setInputMode("ptt");
    view.setInputMode("vad");
    await view.stop(); // Drain controls without opening native audio.
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "voice_set_input_mode").map(([, args]) => args)).toEqual([
      { owner: 1, mode: "vad", thresholdDb: -72 },
      { owner: 1, mode: "ptt", thresholdDb: -72 },
      { owner: 1, mode: "vad", thresholdDb: -72 },
    ]);
    view.setInputSensitivity(null);
    await view.stop();
    expect(bridge.invoke).toHaveBeenCalledWith("voice_set_input_mode", { owner: 1, mode: "vad", thresholdDb: null });
    view.setInputSensitivity(-50);
    await view.stop();
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "voice_set_input_mode").at(-1)?.[1]).toEqual({ owner: 1, mode: "vad", thresholdDb: -50 });
    expect(bridge.invoke).not.toHaveBeenCalledWith("voice_start", expect.anything());
  });

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
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_start")).toHaveLength(0);
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
  it("reads permission without opening or requesting capture", async () => {
    const view = engine();
    expect(await view.micPermission()).toBe("granted");
    expect(bridge.invoke.mock.calls.map(([command]) => command)).toEqual(["voice_bootstrap", "mic_permission"]);
  });

  it("applies queued unmute controls before acquiring preview capture", async () => {
    const unmuted = deferred<void>(), entered = deferred<void>();
    const invoke = bridge.invoke.getMockImplementation()!;
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "voice_set_muted" && args?.muted === false) { entered.resolve(); await unmuted.promise; }
      return invoke(command, args);
    });
    const view = engine();
    view.setMuted(true);
    view.setMuted(false);
    const preview = view.startMicTest(vi.fn());
    await entered.promise;
    expect(bridge.invoke.mock.calls.some(([command]) => command === "mic_test_start")).toBe(false);
    unmuted.resolve();
    const stop = await preview;
    expect(bridge.invoke).toHaveBeenCalledWith("mic_test_start", { owner: 1, input: null, test: 1 });
    stop();
  });

  it("retires and drains a pending preview before voice starts", async () => {
    const opened = deferred<void>(), entered = deferred<void>();
    const invoke = bridge.invoke.getMockImplementation()!;
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "mic_test_start") { entered.resolve(); await opened.promise; }
      return invoke(command, args);
    });
    const view = engine();
    const preview = view.startMicTest(vi.fn());
    await entered.promise;
    const starting = view.start(params());
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1, test: 1 }));
    expect(bridge.invoke.mock.calls.some(([command]) => command === "voice_start")).toBe(false);
    opened.resolve();
    await preview;
    await starting;
    const commands = bridge.invoke.mock.calls.map(([command]) => command);
    expect(commands.lastIndexOf("mic_test_stop")).toBeLessThan(commands.indexOf("voice_start"));
  });

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
    const replacement = view.startMicTest(currentLevel);
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_start")).toHaveLength(1);
    opening.resolve();
    const stopCurrent = await replacement;
    const test = bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_start").at(-1)![1]!.test;
    expect(oldLevel).toHaveBeenLastCalledWith(0);
    oldLevel.mockClear();
    emit("voice://test-level", { owner: 1, test, db: -30 });
    expect(oldLevel).not.toHaveBeenCalled();
    expect(currentLevel).toHaveBeenCalledWith(0.7);
    const stopOld = await oldTest;
    stopOld();
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_stop").every(([, args]) => args?.test === 1)).toBe(true);
    emit("voice://test-level", { owner: 1, test, db: -30 });
    expect(oldLevel).not.toHaveBeenCalled();
    expect(currentLevel).toHaveBeenCalledWith(0.7);
    stopCurrent();
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1, test }));
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
    const replacement = view.startMicTest(vi.fn());
    listening.resolve();
    const stopCurrent = await replacement;
    const stopOld = await oldTest;
    stopOld();
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_start")).toHaveLength(1);
    expect(bridge.invoke.mock.calls.filter(([command]) => command === "mic_test_stop")).toHaveLength(0);
    stopCurrent();
    await vi.waitFor(() => expect(bridge.invoke).toHaveBeenCalledWith("mic_test_stop", { owner: 1, test: 2 }));
  });
});

describe("native encrypted runs", () => {
  const b64 = (n: number, v: number) => btoa(String.fromCharCode(...new Array<number>(n).fill(v))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  const binding = { server_key: b64(32, 1), session: 7, channel: 3, call: b64(16, 2), participation: b64(16, 3), group: b64(32, 4) };
  function keySource(resume: string | null = null) {
    const attached: { run: number; control: (body: never) => Promise<unknown>; close(): void }[] = [];
    const statuses: unknown[] = [];
    return {
      attached, statuses,
      source: { resumeAfter: () => resume, attach: (media: never) => { attached.push(media); }, engineStatus: (s: unknown) => { statuses.push(s); } },
    };
  }

  it("binds the run before negotiation, attaches after it and relays controls with that run", async () => {
    const view = engine();
    const keys = keySource("41");
    await view.start({ ...params(), mode: "encrypted", binding, keys: keys.source as never });
    const start = bridge.invoke.mock.calls.find(([command]) => command === "voice_start")!;
    expect(start[1]).toMatchObject({ mode: "encrypted", binding, resumeAfter: "41" });
    const run = start[1].run as number;
    expect(keys.attached.map(a => a.run)).toEqual([run]);
    const original = bridge.invoke.getMockImplementation()!;
    bridge.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) =>
      command === "voice_set_keys" ? { serial: "1", epoch: "5", result: "applied" } : original(command, args));
    const body = { action: "supersede", serial: "1", epoch: "5" };
    await expect(keys.attached[0]!.control(body as never)).resolves.toEqual({ serial: "1", epoch: "5", result: "applied" });
    expect(bridge.invoke).toHaveBeenCalledWith("voice_set_keys", { owner: 1, run, body });
    expect(await view.capabilities.encryptedVoice()).toBe(true);
  });

  it("refuses encrypted mode without keys or a full binding", async () => {
    const view = engine();
    await expect(view.start({ ...params(), mode: "encrypted", binding })).rejects.toMatchObject({ kind: "unsupported" });
    await expect(view.start({ ...params(), mode: "encrypted", binding: { ...binding, group: null }, keys: keySource().source as never })).rejects.toMatchObject({ kind: "unsupported" });
    expect(bridge.invoke.mock.calls.some(([command]) => command === "voice_start")).toBe(false);
  });

  it("forwards crypto statuses of the current run only; failed closes the engine state", async () => {
    const view = engine();
    const keys = keySource();
    const states: string[] = [];
    view.on("state", s => states.push(s));
    await view.start({ ...params(), mode: "encrypted", binding, keys: keys.source as never });
    const run = keys.attached[0]!.run;
    const status = { binding: { ...binding, run }, state: "securing", reason: "key_expired" };
    emit("voice://crypto", { owner: 1, run: run - 1, ...status });
    emit("voice://crypto", { owner: 2, run, ...status });
    expect(keys.statuses).toEqual([]);
    emit("voice://crypto", { owner: 1, run, ...status });
    expect(keys.statuses).toEqual([status]);
    emit("voice://crypto", { owner: 1, run, ...status, state: "failed", reason: "key_conflict" });
    expect(states.at(-1)).toBe("failed");
  });

  it("closing the call controller stops this run, not a newer one", async () => {
    const view = engine();
    const first = keySource(), second = keySource();
    await view.start({ ...params(), mode: "encrypted", binding, keys: first.source as never });
    await view.start({ ...params(), mode: "encrypted", binding, keys: second.source as never });
    const stops = () => bridge.invoke.mock.calls.filter(([command]) => command === "voice_stop").length;
    const before = stops();
    first.attached[0]!.close();
    expect(stops()).toBe(before);
    second.attached[0]!.close();
    await vi.waitFor(() => expect(stops()).toBe(before + 1));
  });
});

describe("native device changes", () => {
  it("moves running voice to a chosen device without renegotiating", async () => {
    const view = engine();
    await view.start(params());
    bridge.invoke.mockClear();
    await view.setInputDevice("USB mic");
    await view.setOutputDevice(null);
    await view.setOutputDevice("Headphones");
    const devices = () => bridge.invoke.mock.calls.filter(([command]) => command === "voice_set_devices");
    await vi.waitFor(() => expect(devices()).toHaveLength(2));
    expect(devices()).toEqual([
      ["voice_set_devices", { input: "USB mic", output: null, owner: 1 }],
      ["voice_set_devices", { input: "USB mic", output: "Headphones", owner: 1 }],
    ]);
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(bridge.invoke).not.toHaveBeenCalledWith("voice_start", expect.anything());
  });

  it("tells the user where audio moved and clears a problem that is over", async () => {
    const view = engine();
    const switched = vi.fn();
    const micError = vi.fn();
    const outputError = vi.fn();
    view.on("deviceSwitched", switched);
    view.on("micError", micError);
    view.on("outputError", outputError);
    await view.start(params());
    micError.mockClear();
    outputError.mockClear();
    emit("voice://device", { owner: 1, side: "input", kind: "switched", device: "MacBook mic" });
    emit("voice://device", { owner: 1, side: "output", kind: "recovered", device: null });
    emit("voice://device", { owner: 0, side: "input", kind: "switched", device: "Obsolete" });
    expect(switched).toHaveBeenCalledExactlyOnceWith({ side: "input", label: "MacBook mic" });
    expect(outputError).toHaveBeenCalledExactlyOnceWith(null);
    expect(micError).not.toHaveBeenCalled();
  });
});
