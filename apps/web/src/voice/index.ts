import { isDesktop } from "../platform";
import type { VoiceEngine } from "./engine";
import { NativeVoiceEngine } from "./native-engine";
import { WebRtcVoiceEngine } from "./webrtc-engine";

/**
 * Browser: WebRTC + WebAudio. Tauri desktop app: the native Rust engine.
 * UI code only ever sees the `VoiceEngine` interface.
 */
export function createVoiceEngine(): VoiceEngine {
  return isDesktop() ? new NativeVoiceEngine() : new WebRtcVoiceEngine();
}
