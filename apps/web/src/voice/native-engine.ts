import { nextMediaRun } from "./engine";
import { hasInputGateControl } from "./sdp";
import { MicTests } from "./mic-test";
import { dbToLevel, inputThreshold } from "./input-sensitivity";
export { dbToLevel } from "./input-sensitivity";
import { AUDIO_SLOTS } from "../net/protocol";
import { tauri, type TauriBridge, type Unlisten } from "../platform";
import {
  VoiceError,
  type AudioDevice,
  type CaptureOptions,
  type DeviceList,
  type InputMode,
  type MicTestPlayback,
  type VoiceEngine,
  type VoiceEvents,
  type VoiceState,
} from "./engine";
import type { ControlResult, CryptoStatus } from "./sframe/state";
import type { SoundCue } from "../sound/catalog";

type Handler = (value: never) => void;

const CONNECT_TIMEOUT_MS = 15_000;
const DEVICE_POLL_MS = 4_000;

/** `voice://device`: the Rust engine sorted a device out by itself. */
interface DevicePayload {
  owner: number;
  side: "input" | "output";
  kind: "switched" | "recovered";
  device: string | null;
}

/** `voice://issue`: a device problem reported by the Rust engine. */
interface IssuePayload {
  owner: number;
  side: "input" | "output";
  kind: "no_device" | "permission_denied" | "failed";
  message: string;
}

export function micErrorKind(kind: IssuePayload["kind"]): "no_mic" | "mic_denied" | "mic_failed" {
  return kind === "no_device" ? "no_mic" : kind === "permission_denied" ? "mic_denied" : "mic_failed";
}

type StartParams = Parameters<VoiceEngine["start"]>[0];

/**
 * Desktop voice engine. Audio, Opus and WebRTC live in Rust (`gwar-client`);
 * this class is a thin remote control for it. The SDP exchange still goes over
 * the UI's own WebSocket: Rust emits `voice://offer`, we send `voice.offer`
 * and return the answer with `voice_answer`.
 */
export class NativeVoiceEngine implements VoiceEngine {
  // SFrame runs in Rust (`gwar_client::voice::sframe`); keys arrive through `voice_set_keys`.
  readonly capabilities: VoiceEngine["capabilities"] = { outputDeviceSelection: true, encryptedVoice: async () => true };

  private handlers = new Map<string, Set<Handler>>();
  // Bootstrap once per engine: a fresh view retires native audio left by the
  // previous view. Never acquire another token after this owner is superseded.
  private readonly ready: Promise<{ api: TauriBridge; owner: number }>;
  private listening: Promise<void> | null = null;
  private unlisten: Unlisten[] = [];
  private queue: Promise<void> = Promise.resolve();

  private runId = 0;
  private params: StartParams | null = null;
  private state: VoiceState = "idle";
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private devicePoll: ReturnType<typeof setInterval> | null = null;
  private lastDevices = "";
  private smoothed = 0;
  private inputMode: InputMode = "vad";
  private automaticSensitivity = true;
  private threshold = inputThreshold(null);
  private micTestId = 0;
  private micTests = new MicTests();
  private muted = false;
  private deafened = false;
  private gateSupported: boolean | null = null;
  private gateSerial = 0;
  private gateKind = "automatic";
  private gateQueue: Promise<void> = Promise.resolve();

  private inputDevice: string | null = null;
  private outputDevice: string | null = null;
  private userVolumes = new Map<string, number>();
  private slotOwners: Array<string | null> = Array.from({ length: AUDIO_SLOTS }, () => null);
  private sentGain: Array<number | null> = Array.from({ length: AUDIO_SLOTS }, () => null);

  /** `bridge` is the desktop shell; the e2e suite substitutes a native fixture process. */
  constructor(bridge: () => Promise<TauriBridge> = tauri) {
    this.ready = bridge().then(async (api) => ({ api, owner: await api.invoke<number>("voice_bootstrap") }));
    void this.ready.catch((e: unknown) => console.warn("[native voice] bootstrap failed:", e));
  }

  // ---------------------------------------------------------------- events

  playSound(cue: SoundCue, volume: number): void {
    // Preserve ordering with mute/deafen and stop, including outside a call.
    void this.send("voice_play_cue", { cue, volume }, performance.now() + 300);
  }

  on<E extends keyof VoiceEvents>(event: E, handler: (value: VoiceEvents[E]) => void): () => void {
    let set = this.handlers.get(event);
    if (!set) this.handlers.set(event, (set = new Set()));
    set.add(handler as Handler);
    return () => set.delete(handler as Handler);
  }

  private emit<E extends keyof VoiceEvents>(event: E, value: VoiceEvents[E]): void {
    for (const h of this.handlers.get(event) ?? []) h(value as never);
  }

  private setState(state: VoiceState): void {
    if (this.state === state) return;
    this.state = state;
    this.emit("state", state);
  }

  // ------------------------------------------------------------------ plumbing

  private async invoke<T = void>(command: string, args?: Record<string, unknown>): Promise<T> {
    const { api, owner } = await this.ready;
    return api.invoke<T>(command, { ...args, owner });
  }

  /** Commands run in issue order after the one-time ownership bootstrap. */
  private send(command: string, args?: Record<string, unknown>, freshUntil = Infinity): Promise<void> {
    this.queue = this.queue
      .then(async () => {
        await this.ready;
        if (performance.now() <= freshUntil) await this.invoke(command, args);
      })
      .catch((e: unknown) => console.warn(`[native voice] ${command} failed:`, e));
    return this.queue;
  }

  private sendStrict(command: string, args: Record<string, unknown>): Promise<void> {
    const result = this.queue.then(() => this.invoke(command, args));
    this.queue = result.catch(() => {});
    return result;
  }

  private async ensureListeners(): Promise<TauriBridge> {
    const { api, owner } = await this.ready;
    this.listening ??= (async () => {
      this.unlisten.push(
        await api.listen<{ owner: number; run: number; sdp: string }>("voice://offer", (p) => { if (p.owner === owner) void this.onOffer(p.run, p.sdp); }),
        await api.listen<{ owner: number; run: number; state: "connected" | "failed" }>("voice://state", (p) => {
          if (p.owner !== owner || p.run !== this.runId) return;
          if (p.state === "connected") {
            this.clearConnectTimer();
            this.setState("connected");
          } else if (this.state !== "idle") {
            this.setState("failed");
          }
        }),
        await api.listen<{ owner: number; db: number; transmitting: boolean }>("voice://level", (p) => {
          if (p.owner !== owner || this.state === "idle") return;
          this.emit("level", this.shape(dbToLevel(p.db)));
        }),
        await api.listen<{ owner: number; run: number } & CryptoStatus>("voice://crypto", (p) => {
          if (p.owner !== owner || p.run !== this.runId) return;
          // A status is never an installation acknowledgement.
          this.params?.keys?.engineStatus({ binding: p.binding, state: p.state, reason: p.reason });
          if (p.state === "failed") {
            this.clearConnectTimer();
            this.setState("failed");
          }
        }),
        await api.listen<IssuePayload>("voice://issue", (p) => {
          if (p.owner !== owner) return;
          if (p.side === "input") {
            this.emit("micError", new VoiceError(micErrorKind(p.kind), p.message));
          } else {
            this.emit("outputError", new VoiceError("output_failed", p.message));
          }
        }),
        await api.listen<DevicePayload>("voice://device", (p) => {
          if (p.owner !== owner) return;
          if (p.kind === "recovered") this.emit(p.side === "input" ? "micError" : "outputError", null);
          else if (p.device) this.emit("deviceSwitched", { side: p.side, label: p.device });
        }),
      );
    })();
    await this.listening;
    return api;
  }

  private shape(target: number): number {
    // Fast attack, slower release: readable without flicker.
    this.smoothed = target > this.smoothed ? target : this.smoothed * 0.8 + target * 0.2;
    return this.smoothed < 0.01 ? 0 : this.smoothed;
  }

  private async onOffer(run: number, sdp: string): Promise<void> {
    const params = this.params;
    if (run !== this.runId || !params) return;
    try {
      const answer = await params.offer(sdp, run);
      if (run !== this.runId || this.params !== params) return;
      this.gateSupported = !!params.inputGate && hasInputGateControl(answer);
      this.emit("inputGateSupport", this.gateSupported);
      this.gateKind = this.inputMode === "ptt" ? "ptt" : this.automaticSensitivity || !this.gateSupported ? "automatic" : "manual";
      await this.sendMode(true);
      if (this.gateSupported) await params.inputGate!(this.gateKind !== "automatic", run);
      if (run !== this.runId || this.params !== params) return;
      if (!this.gateSupported && this.inputMode === "ptt") throw new VoiceError("input_mode_unsupported", "push-to-talk needs an updated server");
      this.send("voice_answer", { run, sdp: answer });
    } catch (e) {
      this.send("voice_answer", { run, error: e instanceof Error ? e.message : String(e) });
    }
  }

  private clearConnectTimer(): void {
    if (this.connectTimer) clearTimeout(this.connectTimer);
    this.connectTimer = null;
  }

  // ------------------------------------------------------------- lifecycle

  async start(params: StartParams): Promise<void> {
    const encrypted = params.mode === "encrypted";
    const { server_key, call, participation, group } = params.binding;
    if (encrypted && (!params.keys || !server_key || !call || !participation || !group)) throw new VoiceError("unsupported", "encrypted voice needs its call binding and keys");
    const run = (this.runId = nextMediaRun());
    this.params = params;
    this.gateSupported = null;
    this.gateSerial = 0;
    this.gateQueue = Promise.resolve();
    this.emit("inputGateSupport", null);
    this.clearConnectTimer();
    this.setState("starting");
    await this.micTests.stop();
    if (run !== this.runId) return;
    this.emit("micError", null);
    this.emit("outputError", null);
    await this.ensureListeners();
    if (run !== this.runId) return;
    this.sentGain.fill(null);
    try {
      // The engine is bound to this run before negotiation and starts closed.
      const resumeAfter = encrypted ? params.keys!.resumeAfter() : null;
      await this.invoke("voice_start", { run, serverHost: params.serverHost ?? "", mode: params.mode, binding: params.binding, resumeAfter });
    } catch (e) {
      if (run !== this.runId) return;
      this.setState("failed");
      throw new VoiceError("negotiation", typeof e === "string" ? e : e instanceof Error ? e.message : String(e));
    }
    if (run !== this.runId) return;
    // Controls only after the server bound this run: acks name it. Replies
    // arrive after the native RTC task applied each control.
    if (encrypted) {
      params.keys!.attach({
        run,
        control: body => this.invoke<ControlResult>("voice_set_keys", { run, body }),
        close: () => { if (this.runId === run) void this.stop(); },
      });
    }
    this.applyAllSlotGains();
    this.startDevicePoll();
    // `voice://state connected` may already have arrived while we awaited.
    if (this.state === "starting") {
      this.connectTimer = setTimeout(() => {
        if (this.runId === run && this.state === "starting") this.setState("failed");
      }, CONNECT_TIMEOUT_MS);
    }
  }

  async stop(): Promise<void> {
    const stoppedRun = this.runId;
    this.runId = nextMediaRun();
    const retired = this.runId;
    this.params = null;
    this.gateSupported = null;
    this.emit("inputGateSupport", null);
    this.clearConnectTimer();
    this.stopDevicePoll();
    this.smoothed = 0;
    this.emit("level", 0);
    await this.micTests.stop();
    await this.send("voice_stop", { run: stoppedRun });
    if (this.runId === retired) this.setState("idle");
  }

  async cryptoCounters(): Promise<Record<string, number> | null> {
    if (this.params?.mode !== "encrypted") return null;
    return this.invoke<Record<string, number> | null>("voice_crypto_counters", { run: this.runId });
  }

  /** Re-opens the audio devices (and renegotiates), e.g. after plugging in a microphone. */
  async retryMic(): Promise<void> {
    await this.restart();
  }

  private async restart(): Promise<void> {
    const params = this.params;
    if (!params) return;
    try {
      await this.start(params);
    } catch {
      /* start() already reported "failed"; the controller retries */
    }
  }

  // ----------------------------------------------------------------- setters

  setMuted(muted: boolean): void {
    this.muted = muted;
    if (muted) void this.micTests.stop();
    this.send("voice_set_muted", { muted });
  }

  setDeafened(deafened: boolean): void {
    this.deafened = deafened;
    if (deafened) void this.micTests.stop();
    this.send("voice_set_deafened", { deafened });
  }

  setInputMode(mode: InputMode): void {
    this.inputMode = mode;
    const kind = mode === "ptt" ? "ptt" : this.automaticSensitivity || this.gateSupported === false ? "automatic" : "manual";
    if (!this.params || this.gateSupported === null || kind === this.gateKind) { void this.sendMode(); return; }
    this.gateKind = kind;
    const run = this.runId, serial = ++this.gateSerial, params = this.params;
    // Close before applying the local mode. A stale ack can never reopen it.
    const closed = this.sendStrict("voice_set_gate_ready", { run, serial, ready: false });
    const applied = this.sendMode(true);
    this.gateQueue = this.gateQueue.then(async () => {
      await closed; await applied;
      if (run !== this.runId || serial !== this.gateSerial || this.params !== params) return;
      if (this.gateSupported) await params.inputGate!(kind !== "automatic", run);
      else if (kind === "ptt") { this.emit("micError", new VoiceError("input_mode_unsupported", "push-to-talk needs an updated server")); return; }
      if (run === this.runId && serial === this.gateSerial && this.params === params) await this.invoke("voice_set_gate_ready", { run, serial, ready: true });
    }).catch(error => {
      if (run !== this.runId || this.params !== params) return;
      console.warn("[native voice] input mode transition failed:", error);
      this.runId = nextMediaRun();
      this.params = null;
      this.clearConnectTimer();
      void this.send("voice_stop", { run });
      this.setState("failed");
    });
  }

  private sendMode(strict = false): Promise<void> {
    return (strict ? this.sendStrict.bind(this) : this.send.bind(this))("voice_set_input_mode", { mode: this.inputMode, thresholdDb: this.automaticSensitivity || this.gateSupported === false ? null : this.threshold });
  }

  setInputSensitivity(threshold: number | null): void {
    this.automaticSensitivity = threshold === null;
    this.threshold = inputThreshold(threshold);
    this.setInputMode(this.inputMode);
  }

  setPttActive(active: boolean): void {
    this.send("voice_set_ptt", { active });
  }

  // Running voice moves to the new device natively, without renegotiating.
  async setInputDevice(deviceId: string | null): Promise<void> {
    if (this.inputDevice === deviceId) return;
    this.inputDevice = deviceId;
    this.send("voice_set_devices", { input: this.inputDevice, output: this.outputDevice });
  }

  async setOutputDevice(deviceId: string | null): Promise<void> {
    if (this.outputDevice === deviceId) return;
    this.outputDevice = deviceId;
    this.send("voice_set_devices", { input: this.inputDevice, output: this.outputDevice });
  }

  async setCaptureOptions(options: CaptureOptions): Promise<void> {
    await this.send("voice_set_capture_options", {
      echoCancellation: options.echoCancellation,
      autoGainControl: options.autoGainControl,
      noiseSuppression: options.noiseSuppression,
      strength: Math.round(Math.min(100, Math.max(10, options.noiseStrength))),
    });
  }

  setMasterVolume(volume: number): void {
    this.send("voice_set_master_gain", { gain: Math.min(1.5, Math.max(0, volume)) });
  }

  setUserVolume(uid: string, volume: number): void {
    this.userVolumes.set(uid, Math.min(2, Math.max(0, volume)));
    this.slotOwners.forEach((owner, slot) => {
      if (owner === uid) this.applySlotGain(slot);
    });
  }

  setSlotOwner(slot: number, uid: string | null): void {
    if (slot < 0 || slot >= AUDIO_SLOTS) return;
    this.slotOwners[slot] = uid;
    this.applySlotGain(slot);
  }

  private applySlotGain(slot: number): void {
    const owner = this.slotOwners[slot] ?? null;
    const gain = owner ? (this.userVolumes.get(owner) ?? 1) : 1;
    if (this.sentGain[slot] === gain) return;
    this.sentGain[slot] = gain;
    this.send("voice_set_slot_gain", { slot, gain });
  }

  private applyAllSlotGains(): void {
    for (let slot = 0; slot < AUDIO_SLOTS; slot++) this.applySlotGain(slot);
  }

  // ----------------------------------------------------------------- devices

  async listDevices(): Promise<DeviceList> {
    try {
      const { api } = await this.ready;
      const d = await api.invoke<{ inputs: string[]; outputs: string[] }>("audio_devices");
      const map = (names: string[]): AudioDevice[] => names.map((n) => ({ id: n, label: n }));
      return { inputs: map(d.inputs), outputs: map(d.outputs) };
    } catch (e) {
      console.warn("[native voice] audio_devices failed:", e);
      return { inputs: [], outputs: [] };
    }
  }

  private startDevicePoll(): void {
    if (this.devicePoll) return;
    this.devicePoll = setInterval(() => {
      void this.listDevices().then((d) => {
        const key = JSON.stringify(d);
        if (key !== this.lastDevices) {
          this.lastDevices = key;
          this.emit("devices", d);
        }
      });
    }, DEVICE_POLL_MS);
  }

  private stopDevicePoll(): void {
    if (this.devicePoll) clearInterval(this.devicePoll);
    this.devicePoll = null;
  }

  micPermission(): Promise<import("./engine").MicPermission> {
    return this.invoke("mic_permission");
  }

  startMicTest(onLevel: (level: number) => void, signal?: AbortSignal, playback?: MicTestPlayback): Promise<() => void> {
    return this.micTests.start(async (aborted, complete) => {
      if (this.state !== "idle" || this.muted || this.deafened || aborted.aborted) return () => {};
      return this.openMicTest(onLevel, aborted, complete, playback);
    }, signal);
  }

  private async openMicTest(onLevel: (level: number) => void, signal: AbortSignal, complete: () => Promise<boolean>, playback?: MicTestPlayback): Promise<() => void | Promise<void>> {
    const test = ++this.micTestId;
    // Apply queued mute/device controls before the native preview checks its capture policy.
    await this.queue;
    const api = await this.ensureListeners();
    const { owner } = await this.ready;
    let smooth = 0;
    const off = await api.listen<{ owner: number; test: number; db: number }>("voice://test-level", (p) => {
      if (p.owner !== owner || p.test !== test || test !== this.micTestId || signal.aborted) return;
      const target = dbToLevel(p.db);
      smooth = target > smooth ? target : smooth * 0.8 + target * 0.2;
      onLevel(smooth < 0.01 ? 0 : smooth);
    });
    const offState = playback ? await api.listen<{ owner: number; test: number; state: "recording" | "preparing" | "playing" | "idle"; error?: string; error_kind?: "capture_shutdown" }>("voice://test-state", p => {
      if (p.owner !== owner || p.test !== test || test !== this.micTestId || signal.aborted) return;
      if (p.error) playback.error(new VoiceError(p.error_kind === "capture_shutdown" ? "mic_stop_failed" : "output_failed", p.error));
      else if (p.state === "idle") void complete().then(current => { if (current) playback.phase("idle"); });
      else playback.phase(p.state);
    }) : () => {};
    // Listener setup can finish after a newer test has already opened capture.
    if (test !== this.micTestId || signal.aborted) {
      off(); offState();
      return () => {};
    }
    const retire = () => { void this.invoke("mic_test_stop", { test }).catch(() => {}); };
    signal.addEventListener("abort", retire, { once: true });
    try {
      await this.invoke("mic_test_start", { input: this.inputDevice, test, playback: !!playback });
    } catch (e) {
      off(); offState();
      signal.removeEventListener("abort", retire);
      if (signal.aborted) { await this.invoke("mic_test_stop", { test }).catch(() => {}); return () => {}; }
      throw new VoiceError("mic_failed", typeof e === "string" ? e : String(e));
    }
    void this.listDevices().then((d) => this.emit("devices", d));
    return () => {
      off(); offState();
      signal.removeEventListener("abort", retire);
      // A late result belongs to its own test, never the replacement's capture.
      if (test === this.micTestId) { ++this.micTestId; onLevel(0); }
      return this.invoke("mic_test_stop", { test }).catch(() => {});
    };
  }
}
