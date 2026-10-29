import { afterEach, expect, it, vi } from "vitest";
import { WebRtcVoiceEngine } from "../voice/webrtc-engine";
import { NativeVoiceEngine } from "../voice/native-engine";

afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); });

it("routes browser cues through the selected sink after voice deafen/gain, never through capture", async () => {
  const gain = () => ({ connect: vi.fn(), disconnect: vi.fn(), gain: { setTargetAtTime: vi.fn() } });
  const gains: ReturnType<typeof gain>[] = [];
  const sources: { connect: ReturnType<typeof vi.fn>; start: ReturnType<typeof vi.fn>; stop: ReturnType<typeof vi.fn> }[] = [];
  let state = "running";
  const sink = vi.fn(async () => {}), capture = vi.fn();
  class Context {
    currentTime = 0;
    destination = {};
    get state() { return state; }
    setSinkId = sink;
    resume = vi.fn(async () => {});
    createGain() { const node = gain(); gains.push(node); return node; }
    createBuffer(_channels: number, length: number) { return { length, copyToChannel: vi.fn() }; }
    createBufferSource() { const node = { connect: vi.fn(), start: vi.fn(), stop: vi.fn(), disconnect: vi.fn(), onended: null }; sources.push(node); return node; }
    createMediaStreamSource = capture;
  }
  vi.stubGlobal("AudioContext", Context);
  vi.stubGlobal("window", { addEventListener: vi.fn() });
  vi.stubGlobal("navigator", { mediaDevices: { getUserMedia: capture } });
  const e = new WebRtcVoiceEngine();
  await e.setOutputDevice("headphones");
  e.setDeafened(true); e.setMasterVolume(0);
  e.playSound("deafen", 0.6);
  expect(sink).toHaveBeenCalledWith("headphones");
  expect(gains[0]!.gain.setTargetAtTime).toHaveBeenLastCalledWith(0, 0, 0.015);
  expect(sources[0]!.connect).toHaveBeenCalledWith(gains[1]);
  expect(gains[0]!.connect).toHaveBeenCalledWith(gains[1]);
  expect(sources[0]!.start).toHaveBeenCalledOnce();
  expect(capture).not.toHaveBeenCalled();
  await e.stop(); // Leaving voice must not silence a subsequent leave cue.
  e.playSound("selfLeave", 0.6);
  expect(sources).toHaveLength(1); // Deafen also applies at the playback boundary.
  e.setDeafened(false);
  e.playSound("connectionLost", 0.6);
  e.playSound("shareStopped", 0.6);
  expect(sources).toHaveLength(3);
  expect(sources[0]!.stop).toHaveBeenCalledOnce(); // Never more than two simultaneous cues.
  e.setDeafened(true);
  expect(sources[1]!.stop).toHaveBeenCalledOnce();
  expect(sources[2]!.stop).toHaveBeenCalledOnce();
  e.setDeafened(false);
  state = "suspended";
  e.playSound("mention", 0.6);
  expect(sources).toHaveLength(3); // Autoplay does not retain a stale notification.
});

it("sends desktop cues in order after selected-output and deafen controls, with the current owner", async () => {
  const invoke = vi.fn(async (command: string) => command === "voice_bootstrap" ? 7 : undefined);
  const e = new NativeVoiceEngine(async () => ({ invoke: invoke as never, listen: async () => () => {} }));
  await e.setOutputDevice("Headphones");
  e.setDeafened(true); e.playSound("deafen", 0.4);
  await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("voice_play_cue", { owner: 7, cue: "deafen", volume: 0.4 }));
  const commands = invoke.mock.calls.map(([command]) => command);
  expect(commands.indexOf("voice_set_devices")).toBeLessThan(commands.indexOf("voice_play_cue"));
  expect(commands.indexOf("voice_set_deafened")).toBeLessThan(commands.indexOf("voice_play_cue"));
  await e.stop();
});

it("discards a desktop cue that became stale while native ownership was starting", async () => {
  vi.useFakeTimers();
  let bootstrap!: (owner: number) => void;
  const invoke = vi.fn((command: string) => command === "voice_bootstrap" ? new Promise<number>(resolve => { bootstrap = resolve; }) : Promise.resolve());
  const e = new NativeVoiceEngine(async () => ({ invoke: invoke as never, listen: async () => () => {} }));
  e.playSound("mention", 0.6);
  await vi.advanceTimersByTimeAsync(500);
  bootstrap(7);
  await vi.advanceTimersByTimeAsync(0);
  expect(invoke.mock.calls.map(([command]) => command)).toEqual(["voice_bootstrap"]);
  await e.stop();
});
