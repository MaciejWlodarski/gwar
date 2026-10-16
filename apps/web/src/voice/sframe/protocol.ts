/** Messages between the encrypted voice engine and its dedicated SFrame worker. */
import type { ControlResult, Counters, CryptoStatus, VoiceKeyBinding } from "./state";

export type ToWorker =
  | { type: "probe" }
  | { type: "start"; binding: VoiceKeyBinding; resume_after: string | null }
  | { type: "control"; id: number; body: unknown; bytes: number }
  | { type: "stats"; id: number }
  | { type: "stop" };
export type FromWorker =
  | { type: "probe"; crypto: boolean; transformEvents: boolean }
  | { type: "transform"; side: "send" | "receive" }
  | { type: "result"; id: number; result: ControlResult }
  | { type: "status"; status: CryptoStatus }
  | { type: "stats"; id: number; counters: Counters & { queue_dropped: number } };
/** `RTCRtpScriptTransform` options: which side of the run a transform serves. */
export interface TransformOptions { side: "send" | "receive"; slot?: number }
