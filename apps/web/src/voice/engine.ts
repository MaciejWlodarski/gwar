/**
 * The only voice surface the UI may touch. The browser build implements it
 * with WebRTC + WebAudio (`webrtc-engine.ts`); the Tauri desktop app uses a
 * native implementation of the same interface (`native-engine.ts`).
 */
import type { UplinkStatus } from "./uplink-congestion";
import type { IceServer } from "../proto/IceServer";
import type { Control, ControlResult, CryptoStatus } from "./sframe/state";
import { dbToLevel } from "./input-sensitivity";

export type InputMode = "vad" | "ptt";
export type MicPermission = "granted" | "prompt" | "denied" | "unknown";

export interface CaptureOptions {
  noiseSuppression: boolean;
  /** Percent, 10..100: how hard Gwar's own suppression works (the browser's, a fallback, has no setting). */
  noiseStrength: number;
  echoCancellation: boolean;
  autoGainControl: boolean;
}

export type VoiceState = "idle" | "starting" | "connected" | "failed";

export type VoiceErrorKind =
  | "mic_denied" // permission refused
  | "no_mic" // no input device
  | "mic_failed" // device busy / other capture error
  | "insecure" // page is not a secure context, browsers hide the microphone
  | "output_failed" // speakers could not be opened / disappeared (desktop)
  | "negotiation" // WebRTC offer/answer failed
  | "unsupported"
  | "input_mode_unsupported";

export class VoiceError extends Error {
  constructor(
    readonly kind: VoiceErrorKind,
    message: string,
  ) {
    super(message);
    this.name = "VoiceError";
  }
}

/** Exchanges an SDP offer for the server's SDP answer (`voice.offer`). */
export type OfferFn = (sdp: string, run: number) => Promise<string>;
let mediaRun = Date.now() * 1000;
export const nextMediaRun = () => ++mediaRun;
export interface VoiceStartBinding { server_key: string | null; session: number; channel: number; call: string | null; participation: string | null; group: string | null }
/** The MLS controller of an encrypted call, as an engine sees it. */
export interface VoiceKeySource {
  /** For a new engine: the epoch it must never install at or below, or null on a first run. */
  resumeAfter(): string | null;
  /** The run's engine is negotiated: it accepts controls from now on. */
  attach(media: { run: number; control(body: Control): Promise<ControlResult>; close(): void }): void;
  engineStatus(status: CryptoStatus): void;
}

export interface AudioDevice {
  id: string;
  label: string;
}

export interface DeviceList {
  inputs: AudioDevice[];
  outputs: AudioDevice[];
}

export interface VoiceEvents {
  /** Fresh, session-local WebRTC uplink health (browser engine). */
  uplink: UplinkStatus | null;
  /** Local microphone level 0..1 (already shaped for display). */
  level: number;
  /** null before negotiation; false means only the legacy automatic gate is available. */
  inputGateSupport: boolean | null;
  state: VoiceState;
  /** Capture problem; `null` clears it. Does not affect listening. */
  micError: VoiceError | null;
  /** The set of available devices changed. */
  devices: DeviceList;
  /** Playback problem (desktop only); `null` clears it. */
  outputError: VoiceError | null;
  /** Audio moved to another device by itself: the chosen one disappeared or came back. */
  deviceSwitched: { side: "input" | "output"; label: string };
}

export interface VoiceEngine {
  /** Local-only notifications, independent of voice volume, through the voice output device. */
  playSound?(cue: import("../sound/catalog").SoundCue, volume: number): void;
  /** Unlock browser autoplay from a gesture without opening the microphone. */
  unlockSounds?(): void;
  readonly capabilities: {
    /** Output device can be chosen (not just the system default). */
    outputDeviceSelection: boolean;
    /** Whether this engine can join encrypted voice; checked before the microphone and `channel.join`. */
    encryptedVoice(): Promise<boolean>;
  };

  /**
   * Opens the microphone (a failure is reported through `micError` and does
   * NOT reject: the user can still listen) and negotiates the audio transport.
   * Calling it again re-negotiates, e.g. after a reconnect.
   */
  start(params: {
    offer: OfferFn;
    /** Optional server capability: change this run's input gate and await application. */
    inputGate?: (clientGate: boolean, run: number) => Promise<void>;
    mode: "plaintext" | "encrypted";
    binding: VoiceStartBinding;
    /** Required in encrypted mode; there is never a plaintext fallback. */
    keys?: VoiceKeySource;
    iceServers: IceServer[];
    /** Host of the voice server; the native engine resolves it to send media there. */
    serverHost?: string;
  }): Promise<void>;
  stop(): Promise<void>;

  /** Tries to open the microphone again (after the user fixed permissions). */
  retryMic(): Promise<void>;

  setMuted(muted: boolean): void;
  setDeafened(deafened: boolean): void;
  setInputMode(mode: InputMode): void;
  /** null: the legacy automatic decision; otherwise a threshold in dBFS. */
  setInputSensitivity(threshold: number | null): void;
  /** Push-to-talk key state; only matters in `ptt` mode. */
  setPttActive(active: boolean): void;
  setInputDevice(deviceId: string | null): Promise<void>;
  setOutputDevice(deviceId: string | null): Promise<void>;
  setCaptureOptions(options: CaptureOptions): Promise<void>;
  /** 0..1.5 */
  setMasterVolume(volume: number): void;
  /** 0..2, keyed by the user's stable uid. */
  setUserVolume(uid: string, volume: number): void;
  /** Tells the engine whose audio arrives on a receive slot (`voice.slot`). */
  setSlotOwner(slot: number, uid: string | null): void;

  listDevices(): Promise<DeviceList>;
  /** Aggregate SFrame counters of the current encrypted run (never keys), or null. */
  cryptoCounters?(): Promise<Record<string, number> | null>;
  /** Read-only permission check: never opens the microphone or prompts. */
  micPermission(): Promise<MicPermission>;
  /** Local-only capture; cancellation also retires a pending device request. */
  startMicTest(onLevel: (level: number) => void, signal?: AbortSignal): Promise<() => void>;

  on<E extends keyof VoiceEvents>(event: E, handler: (value: VoiceEvents[E]) => void): () => void;
}

/** Maps an RMS amplitude (0..1) to the sensitivity meter's -100..0 dB scale. */
export function rmsToLevel(rms: number): number {
  if (rms <= 0) return 0;
  const db = 20 * Math.log10(rms);
  return dbToLevel(db);
}
