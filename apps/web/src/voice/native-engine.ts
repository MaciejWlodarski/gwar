import type { IceServer } from "../proto/IceServer";
import { AUDIO_SLOTS } from "../net/protocol";
import { tauri, type TauriBridge, type Unlisten } from "../platform";
import {
  VoiceError,
  type AudioDevice,
  type CaptureOptions,
  type DeviceList,
  type InputMode,
  type OfferFn,
  type VoiceEngine,
  type VoiceEvents,
  type VoiceState,
} from "./engine";

type Handler = (value: never) => void;

const CONNECT_TIMEOUT_MS = 15_000;
const DEVICE_POLL_MS = 4_000;

/** `voice://issue`: a device problem reported by the Rust engine. */
interface IssuePayload {
  side: "input" | "output";
  kind: "no_device" | "permission_denied" | "failed";
  message: string;
}

export function micErrorKind(kind: IssuePayload["kind"]): "no_mic" | "mic_denied" | "mic_failed" {
  return kind === "no_device" ? "no_mic" : kind === "permission_denied" ? "mic_denied" : "mic_failed";
}

/** Run ids make stale events from a superseded session harmless. */
let runCounter = 0;

/** dBFS (RMS) -> 0..1 over a 60 dB range, same shaping as the browser meter. */
export function dbToLevel(db: number): number {
  return Math.min(1, Math.max(0, (db + 60) / 60));
}

interface StartParams {
  offer: OfferFn;
  iceServers: IceServer[];
  serverHost?: string;
}

/**
 * Desktop voice engine. Audio, Opus and WebRTC live in Rust (`vc-client`);
 * this class is a thin remote control for it. The SDP exchange still goes over
 * the UI's own WebSocket: Rust emits `voice://offer`, we send `voice.offer`
 * and return the answer with `voice_answer`.
 */
export class NativeVoiceEngine implements VoiceEngine {
  readonly capabilities: VoiceEngine["capabilities"] = { outputDeviceSelection: true, captureProcessing: false };

  private handlers = new Map<string, Set<Handler>>();
  private api: TauriBridge | null = null;
  private listening: Promise<void> | null = null;
  private unlisten: Unlisten[] = [];
  private queue: Promise<unknown> = Promise.resolve();

  private runId = 0;
  private params: StartParams | null = null;
  private state: VoiceState = "idle";
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private devicePoll: ReturnType<typeof setInterval> | null = null;
  private lastDevices = "";
  private restartTimer: ReturnType<typeof setTimeout> | null = null;
  private smoothed = 0;

  private inputDevice: string | null = null;
  private outputDevice: string | null = null;
  private userVolumes = new Map<string, number>();
  private slotOwners: Array<string | null> = Array.from({ length: AUDIO_SLOTS }, () => null);
  private sentGain: Array<number | null> = Array.from({ length: AUDIO_SLOTS }, () => null);

  // ---------------------------------------------------------------- events

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

  /** Fire-and-forget command; commands run in the order they were issued. */
  private send(command: string, args?: Record<string, unknown>): void {
    this.queue = this.queue
      .then(async () => (this.api ??= await tauri()).invoke(command, args))
      .catch((e: unknown) => console.warn(`[native voice] ${command} failed:`, e));
  }

  private async ensureListeners(): Promise<TauriBridge> {
    this.api ??= await tauri();
    const api = this.api;
    this.listening ??= (async () => {
      this.unlisten.push(
        await api.listen<{ run: number; sdp: string }>("voice://offer", (p) => void this.onOffer(p.run, p.sdp)),
        await api.listen<{ run: number; state: "connected" | "failed" }>("voice://state", (p) => {
          if (p.run !== this.runId) return;
          if (p.state === "connected") {
            this.clearConnectTimer();
            this.setState("connected");
          } else if (this.state !== "idle") {
            this.setState("failed");
          }
        }),
        await api.listen<{ db: number; transmitting: boolean }>("voice://level", (p) => {
          if (this.state === "idle") return;
          this.emit("level", this.shape(dbToLevel(p.db)));
        }),
        await api.listen<IssuePayload>("voice://issue", (p) => {
          if (p.side === "input") {
            this.emit("micError", new VoiceError(micErrorKind(p.kind), p.message));
          } else {
            this.emit("outputError", new VoiceError("output_failed", p.message));
          }
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
      const answer = await params.offer(sdp);
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
    const run = (this.runId = ++runCounter);
    this.params = params;
    this.clearConnectTimer();
    this.setState("starting");
    this.emit("micError", null);
    this.emit("outputError", null);
    const api = await this.ensureListeners();
    if (run !== this.runId) return;
    this.sentGain.fill(null);
    try {
      await api.invoke("voice_start", { run, serverHost: params.serverHost ?? "" });
    } catch (e) {
      if (run !== this.runId) return;
      this.setState("failed");
      throw new VoiceError("negotiation", typeof e === "string" ? e : e instanceof Error ? e.message : String(e));
    }
    if (run !== this.runId) return;
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
    this.runId = ++runCounter;
    this.params = null;
    this.clearConnectTimer();
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = null;
    this.stopDevicePoll();
    this.smoothed = 0;
    this.emit("level", 0);
    this.setState("idle");
    this.send("voice_stop");
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

  /** Device changes apply on the next start; restart now if a session is live. */
  private scheduleRestart(): void {
    if (!this.params || this.state === "idle") return;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = setTimeout(() => {
      this.restartTimer = null;
      void this.restart();
    }, 200);
  }

  // ----------------------------------------------------------------- setters

  setMuted(muted: boolean): void {
    this.send("voice_set_muted", { muted });
  }

  setDeafened(deafened: boolean): void {
    this.send("voice_set_deafened", { deafened });
  }

  setInputMode(mode: InputMode): void {
    this.send("voice_set_input_mode", { mode });
  }

  setPttActive(active: boolean): void {
    this.send("voice_set_ptt", { active });
  }

  async setInputDevice(deviceId: string | null): Promise<void> {
    if (this.inputDevice === deviceId) return;
    this.inputDevice = deviceId;
    this.send("voice_set_devices", { input: this.inputDevice, output: this.outputDevice });
    this.scheduleRestart();
  }

  async setOutputDevice(deviceId: string | null): Promise<void> {
    if (this.outputDevice === deviceId) return;
    this.outputDevice = deviceId;
    this.send("voice_set_devices", { input: this.inputDevice, output: this.outputDevice });
    this.scheduleRestart();
  }

  /** Noise suppression / echo cancellation / AGC are not implemented natively yet. */
  async setCaptureOptions(_options: CaptureOptions): Promise<void> {}

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
      const api = (this.api ??= await tauri());
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

  async startMicTest(onLevel: (level: number) => void): Promise<() => void> {
    const api = await this.ensureListeners();
    let smooth = 0;
    const off = await api.listen<{ db: number }>("voice://test-level", (p) => {
      const target = dbToLevel(p.db);
      smooth = target > smooth ? target : smooth * 0.8 + target * 0.2;
      onLevel(smooth < 0.01 ? 0 : smooth);
    });
    try {
      await api.invoke("mic_test_start", { input: this.inputDevice });
    } catch (e) {
      off();
      throw new VoiceError("mic_failed", typeof e === "string" ? e : String(e));
    }
    void this.listDevices().then((d) => this.emit("devices", d));
    return () => {
      off();
      this.send("mic_test_stop");
      onLevel(0);
    };
  }
}
