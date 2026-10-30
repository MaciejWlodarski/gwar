import { RtcUplinkObserver } from "./uplink-congestion";
import { startDenoise, type DenoisePipeline } from "./denoise";
import { nextMediaRun } from "./engine";
import { inputThreshold } from "./input-sensitivity";
import { startInputGate, type CaptureGate } from "./input-gate";
import { advertiseInputGate, hasInputGateControl } from "./sdp";
import { MicTests } from "./mic-test";
import { encryptedVoiceSupported, SframeWorker } from "./sframe/client";
import { AUDIO_SLOTS } from "../net/protocol";
import {
  rmsToLevel,
  VoiceError,
  type AudioDevice,
  type CaptureOptions,
  type DeviceList,
  type InputMode,
  type VoiceEngine,
  type VoiceEvents,
  type VoiceState,
} from "./engine";

type Handler = (value: never) => void;

interface SlotPipe {
  source: MediaStreamAudioSourceNode;
  gain: GainNode;
  element: HTMLAudioElement;
  owner: string | null;
}

type SinkAudioContext = AudioContext & { setSinkId?: (id: string) => Promise<void> };
type SinkAudioElement = HTMLAudioElement & { setSinkId?: (id: string) => Promise<void> };

const CONNECT_TIMEOUT_MS = 15_000;
/** Device notifications come in bursts (a headset appears, then becomes the default). */
const FOLLOW_SETTLE_MS = 250;

/** A real device, not the browser's "default"/"communications" alias of one. */
const realDevice = (d: MediaDeviceInfo, kind: MediaDeviceKind) => d.kind === kind && !!d.deviceId && d.deviceId !== "default" && d.deviceId !== "communications";

/** The device the browser's default alias stands for (same group), or the first one listed. */
function defaultDevice(all: MediaDeviceInfo[], kind: MediaDeviceKind): MediaDeviceInfo | undefined {
  const alias = all.find((d) => d.kind === kind && d.deviceId === "default");
  return (alias?.groupId ? all.find((d) => realDevice(d, kind) && d.groupId === alias.groupId) : undefined) ?? alias ?? all.find((d) => realDevice(d, kind));
}

function trackSettings(stream: MediaStream | null): MediaTrackSettings {
  return stream?.getAudioTracks()[0]?.getSettings?.() ?? {};
}

function deviceList(all: MediaDeviceInfo[]): DeviceList {
  const map = (kind: MediaDeviceKind, fallback: string): AudioDevice[] =>
    all
      .filter((d) => d.kind === kind && d.deviceId !== "default" && d.deviceId !== "communications")
      .map((d, i) => ({ id: d.deviceId, label: d.label || `${fallback} ${i + 1}` }));
  return { inputs: map("audioinput", "Microphone"), outputs: map("audiooutput", "Speaker") };
}

function errorFromGum(e: unknown): VoiceError {
  const name = e instanceof DOMException ? e.name : "";
  if (name === "NotAllowedError" || name === "SecurityError") return new VoiceError("mic_denied", String(e));
  if (name === "NotFoundError" || name === "OverconstrainedError") return new VoiceError("no_mic", String(e));
  return new VoiceError("mic_failed", e instanceof Error ? e.message : String(e));
}

/**
 * Browser voice engine.
 *
 * Transport: one RTCPeerConnection. Transceiver 0 is the microphone
 * (`sendonly`), transceivers 1..8 are `recvonly` "speaker slots". The server
 * maps slot -> user via `voice.slot`; we mix every slot through WebAudio so
 * each user gets an individual GainNode, then a master gain, then the output.
 */
export class WebRtcVoiceEngine implements VoiceEngine {
  readonly capabilities: VoiceEngine["capabilities"];

  private handlers = new Map<string, Set<Handler>>();
  private pc: RTCPeerConnection | null = null;
  private uplinkTimer: ReturnType<typeof setInterval> | null = null;
  private sframe: SframeWorker | null = null;
  private micSender: RTCRtpSender | null = null;
  private stream: MediaStream | null = null;
  /** Granted capture waiting for its gate module; mute/stop must release it too. */
  private openingStream: MediaStream | null = null;
  private ctx: SinkAudioContext | null = null;
  private master: GainNode | null = null;
  private streamDest: MediaStreamAudioDestinationNode | null = null;
  private sinkElement: SinkAudioElement | null = null;
  private analyser: AnalyserNode | null = null;
  private meterSource: MediaStreamAudioSourceNode | null = null;
  private meterTimer: ReturnType<typeof setInterval> | null = null;
  private meterBuf: Float32Array<ArrayBuffer> | null = null;
  private smoothed = 0;
  private pipes: Array<SlotPipe | null> = Array.from({ length: AUDIO_SLOTS }, () => null);
  private slotOwners: Array<string | null> = Array.from({ length: AUDIO_SLOTS }, () => null);
  private userVolumes = new Map<string, number>();

  private muted = false;
  private deafened = false;
  private mode: InputMode = "vad";
  private ptt = false;
  private pttRelease: ReturnType<typeof setTimeout> | null = null;
  private threshold = inputThreshold(null);
  private automaticSensitivity = true;
  private inputGate: CaptureGate | null = null;
  private masterVolume = 1;
  private inputDeviceId: string | null = null;
  private outputDeviceId: string | null = null;
  /** The output actually routed to: the chosen one, or the default while it is away. */
  private routedOutputId: string | null = null;
  /** Each routing attempt's token: a late rejection of an older one changes nothing. */
  private routeAttempt = 0;
  /** The microphone was lost or never found: one appearing is opened at once. */
  private micMissing = false;
  private followTimer: ReturnType<typeof setTimeout> | null = null;
  /** Device follow-ups, one at a time; a run's chain is dropped with it. */
  private following: Promise<void> = Promise.resolve();
  /** An automatic switch that failed is tried again with backoff while the working mic stays. */
  private followRetryTimer: ReturnType<typeof setTimeout> | null = null;
  private followRetries = 0;
  /** The capture a switch replaces, still possibly sent until the replacement lands. */
  private handover: MediaStream | null = null;
  private handoverGate: CaptureGate | null = null;
  private capture: CaptureOptions = { noiseSuppression: true, noiseStrength: 100, echoCancellation: true, autoGainControl: true };
  /**
   * Gwar's own noise suppression (./denoise) once it is loaded; the browser's until then, and for
   * the rest of the page if it cannot run here.
   */
  private denoise: DenoisePipeline | null = null;
  /** The model loading for a run (`runId`), cancelled when that run ends or suppression goes off. */
  private denoiseLoad: { run: number; abort: AbortController } | null = null;
  private denoiseUnavailable = false;
  private previewDenoise: DenoisePipeline | null = null;
  /**
   * The peer's replacements of the sent track, one at a time: `sender.track` changes only once
   * one completes. One a closed peer leaves unsettled is not waited for.
   */
  private sentTrackUpdates: Promise<void> = Promise.resolve();
  private state: VoiceState = "idle";
  private runId = 0;
  private micGeneration = 0;
  private resumeBound = false;
  private micTests = new MicTests();
  private gateSupported: boolean | null = null;
  private meterTrack: MediaStreamTrack | null = null;
  private gateControl: { desired: boolean | null; serial: number; ready: boolean; queue: Promise<void>; apply(clientGate: boolean): Promise<void> } | null = null;

  constructor() {
    this.capabilities = {
      encryptedVoice: () => encryptedVoiceSupported(),
      outputDeviceSelection:
        typeof AudioContext !== "undefined" &&
        ("setSinkId" in AudioContext.prototype || (typeof HTMLMediaElement !== "undefined" && "setSinkId" in HTMLMediaElement.prototype)),
    };
    if (typeof navigator !== "undefined" && navigator.mediaDevices?.addEventListener) {
      navigator.mediaDevices.addEventListener("devicechange", () => this.scheduleFollow());
    }
  }

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

  // ------------------------------------------------------------- lifecycle

  async start({ offer, inputGate, iceServers, mode, binding, keys }: Parameters<VoiceEngine["start"]>[0]): Promise<void> {
    const encrypted = mode === "encrypted";
    const { server_key, call, participation, group } = binding;
    if (encrypted && (!keys || !server_key || !call || !participation || !group)) throw new VoiceError("unsupported", "encrypted voice needs its call binding and keys");
    const mediaRun = nextMediaRun();
    const run = ++this.runId;
    // A model still loading is the run before's.
    this.cancelDenoiseLoad();
    // Closes the old gate and drops its queued audio before the new run starts.
    this.teardownPeer();
    this.setState("starting");
    this.applyTrackEnabled();
    await this.micTests.stop();
    if (run !== this.runId) return;
    if (encrypted && !(await encryptedVoiceSupported())) {
      this.setState("failed");
      throw new VoiceError("unsupported", "Update your browser or use the desktop app to join encrypted voice.");
    }
    if (run !== this.runId) return;
    this.ensureContext();
    await this.ctx?.resume().catch(() => {});
    if (run !== this.runId) return;

    if (!this.stream) await this.openMic();
    if (run !== this.runId) return;
    this.loadDenoise();

    try {
      const pc = new RTCPeerConnection({
        iceServers: iceServers.map((s) => ({
          urls: s.urls,
          ...(s.username ? { username: s.username } : {}),
          ...(s.credential ? { credential: s.credential } : {}),
        })),
        bundlePolicy: "max-bundle",
      });
      this.pc = pc;
      const uplink = new RtcUplinkObserver();
      let polling = false;
      let uplinkGeneration = 0, uplinkConnected = false;
      const pollUplink = async () => {
        if (this.pc !== pc) return;
        if (pc.connectionState !== "connected") { this.emit("uplink", null); return; }
        if (polling) { this.emit("uplink", uplink.observe(performance.now(), null)); return; }
        const generation = uplinkGeneration;
        polling = true;
        try {
          const report = await pc.getStats();
          if (this.pc === pc && generation === uplinkGeneration) this.emit("uplink", pc.connectionState === "connected" ? uplink.observe(performance.now(), report) : null);
        } catch {
          if (this.pc === pc && generation === uplinkGeneration) this.emit("uplink", pc.connectionState === "connected" ? uplink.observe(performance.now(), null) : null);
        } finally { if (generation === uplinkGeneration) polling = false; }
      };
      this.uplinkTimer = setInterval(() => void pollUplink(), 1_000);
      // One dedicated worker per encrypted run; it starts closed.
      const sframe = encrypted ? new SframeWorker({ server_key: server_key!, session: binding.session, channel: binding.channel, call: call!, participation: participation!, run: mediaRun, group: group! }, keys!.resumeAfter(), status => {
        if (this.sframe !== sframe) return;
        keys!.engineStatus(status);
        if (status.state === "failed") { this.teardownPeer(); this.setState("failed"); }
      }) : null;
      this.sframe = sframe;
      const track = this.sentTrack();
      // Transceiver 0 = microphone, created without a track so its transform is
      // attached before any audio; the track follows with replaceTrack.
      const mic = pc.addTransceiver("audio", { direction: "sendonly" });
      if (sframe) mic.sender.transform = sframe.transform({ side: "send" });
      if (track) await mic.sender.replaceTrack(track);
      if (run !== this.runId) return;
      this.micSender = mic.sender;
      // The model may have loaded meanwhile, and the microphone been captured again.
      await this.updateSentTrack();
      if (run !== this.runId) return;
      const slotTransceivers: RTCRtpTransceiver[] = [];
      for (let i = 0; i < AUDIO_SLOTS; i++) {
        const slot = pc.addTransceiver("audio", { direction: "recvonly" });
        if (sframe) slot.receiver.transform = sframe.transform({ side: "receive", slot: i });
        slotTransceivers.push(slot);
      }

      pc.ontrack = (ev) => {
        const slot = slotTransceivers.indexOf(ev.transceiver);
        if (this.pc === pc && slot >= 0) this.attachSlot(slot, ev.track);
      };
      pc.onconnectionstatechange = () => {
        if (this.pc !== pc) return;
        if (uplinkConnected && pc.connectionState !== "connected") {
          uplinkGeneration++; polling = false; uplink.suspend();
          this.emit("uplink", null);
        }
        uplinkConnected = pc.connectionState === "connected";
        if (pc.connectionState === "connected") this.setState("connected");
        else if (pc.connectionState === "failed" || pc.connectionState === "closed") this.setState("failed");
        else if (pc.connectionState === "disconnected") {
          // Transient glitches often recover by themselves.
          setTimeout(() => {
            if (this.pc === pc && pc.connectionState === "disconnected") this.setState("failed");
          }, 5_000);
        }
      };

      const localOffer = await pc.createOffer();
      if (run !== this.runId) return;
      await pc.setLocalDescription(localOffer);
      if (run !== this.runId) return;
      const answer = await offer(advertiseInputGate(pc.localDescription?.sdp ?? ""), mediaRun);
      if (run !== this.runId) return;
      await pc.setRemoteDescription({ type: "answer", sdp: answer });
      if (run !== this.runId) return;
      if (inputGate && hasInputGateControl(answer)) {
        this.gateControl = { desired: null, serial: 0, ready: false, queue: Promise.resolve(), apply: clientGate => inputGate(clientGate, mediaRun) };
      }
      this.gateSupported = this.gateControl !== null;
      this.emit("inputGateSupport", this.gateSupported);
      // Controls only after the server bound this run: acks name it.
      // Closing the call controller stops this run's media synchronously.
      if (sframe) keys!.attach({ run: mediaRun, control: body => sframe.control(body), close: () => { if (this.sframe === sframe) { this.runId++; this.teardownPeer(); this.setState("idle"); } } });
      this.applyTrackEnabled();

      setTimeout(() => {
        if (this.pc === pc && this.state === "starting") this.setState("failed");
      }, CONNECT_TIMEOUT_MS);
    } catch (e) {
      if (run !== this.runId) return;
      this.teardownPeer();
      this.setState("failed");
      throw new VoiceError("negotiation", e instanceof Error ? e.message : String(e));
    }
  }

  async stop(): Promise<void> {
    this.runId++;
    this.cancelPttRelease();
    this.ptt = false;
    this.teardownPeer();
    this.closeMic();
    this.closeDenoise();
    this.setState("idle");
    await this.micTests.stop();
  }

  private teardownPeer(): void {
    this.gateControl = null;
    this.gateSupported = null;
    this.emit("inputGateSupport", null);
    if (this.uplinkTimer) clearInterval(this.uplinkTimer);
    this.uplinkTimer = null;
    this.emit("uplink", null);
    // Invalidate the run's keys first, then the transport.
    this.sframe?.stop();
    this.sframe = null;
    const pc = this.pc;
    this.pc = null;
    this.micSender = null;
    this.sentTrackUpdates = Promise.resolve();
    // A replacement a closed peer leaves unsettled must not hold the next call's device changes.
    this.following = Promise.resolve();
    if (this.followRetryTimer) clearTimeout(this.followRetryTimer);
    this.followRetryTimer = null;
    this.followRetries = 0;
    // The capture a switch was replacing is not sent by a later peer.
    this.handover?.getTracks().forEach((t) => t.stop());
    this.handover = null;
    this.handoverGate?.close();
    this.handoverGate = null;
    if (pc) {
      pc.ontrack = null;
      pc.onconnectionstatechange = null;
      pc.close();
    }
    for (let i = 0; i < this.pipes.length; i++) this.detachSlot(i);
  }

  // ---------------------------------------------------------------- capture

  private audioConstraints(exact: boolean): MediaTrackConstraints {
    return {
      ...(this.inputDeviceId ? { deviceId: exact ? { exact: this.inputDeviceId } : this.inputDeviceId } : {}),
      noiseSuppression: this.capture.noiseSuppression && !this.denoise,
      echoCancellation: this.capture.echoCancellation,
      autoGainControl: this.capture.autoGainControl,
      channelCount: 1,
    };
  }

  private async getStream(current = () => true): Promise<MediaStream> {
    if (!window.isSecureContext || !navigator.mediaDevices?.getUserMedia) {
      throw new VoiceError("insecure", "microphone requires a secure context (https or localhost)");
    }
    try {
      return await navigator.mediaDevices.getUserMedia({ audio: this.audioConstraints(true) });
    } catch (e) {
      // A remembered device may have been unplugged: fall back to the default one.
      if (current() && this.inputDeviceId && e instanceof DOMException && (e.name === "OverconstrainedError" || e.name === "NotFoundError")) {
        return await navigator.mediaDevices.getUserMedia({ audio: { ...this.audioConstraints(false), deviceId: undefined } });
      }
      throw errorFromGum(e);
    }
  }

  /**
   * Captures the microphone. `swap` keeps the current one until the gated replacement is sent,
   * and keeps it when the new capture or gate fails while it still works.
   */
  private async openMic({ swap = false } = {}): Promise<MediaStream | null> {
    if (swap) {
      this.micGeneration++;
      this.openingStream?.getTracks().forEach(track => track.stop());
      this.openingStream = null;
    } else this.closeMic();
    if (this.muted || this.deafened) return null;
    const generation = this.micGeneration;
    const run = this.runId;
    const current = () => generation === this.micGeneration && run === this.runId;
    try {
      const stream = await this.getStream(current);
      if (!current()) {
        stream.getTracks().forEach((track) => track.stop());
        return null;
      }
      // Never attach raw capture while the gate module is loading.
      stream.getAudioTracks().forEach(track => { track.enabled = false; });
      this.openingStream = stream;
      const release = () => {
        if (this.openingStream !== stream) return;
        this.openingStream = null;
        stream.getTracks().forEach(track => track.stop());
      };
      let gate: CaptureGate;
      try {
        gate = await startInputGate(this.ensureContext(), stream);
      } catch (error) {
        release();
        throw error;
      }
      if (!current() || this.muted || this.deafened) {
        gate.close();
        release();
        return null;
      }
      const previous = this.stream;
      const previousGate = this.inputGate;
      this.stopMeter();
      this.inputGate = gate;
      this.openingStream = null;
      this.stream = stream;
      this.micMissing = false;
      // Retargeting the shared denoiser also changes audio carried by the old
      // gate. Close that output before its detector and audio can disagree.
      if (this.denoise) previousGate?.close();
      this.denoise?.setSource(stream);
      this.sentTrack();
      this.emit("micError", null);
      this.applyTrackEnabled();
      const track = stream.getAudioTracks()[0];
      track?.addEventListener("ended", () => {
        // The device went away: move to what is left at once.
        if (this.stream === stream) void this.queueFollow(() => this.swapMic(stream));
      });
      // Labels become available only after permission was granted.
      void this.listDevices().then((d) => { if (current()) this.emit("devices", d); });
      if (swap) {
        // Sent before the previous capture stops, so the call hears no gap beyond the switch;
        // until then mute and push-to-talk gate it too, and a stop ends it.
        this.handover = previous;
        this.handoverGate = this.denoise ? null : previousGate;
        await this.updateSentTrack();
        if (this.handover === previous) this.handover = null;
        if (this.handoverGate === previousGate) {
          this.handoverGate = null;
          previousGate?.close();
        }
        previous?.getTracks().forEach((t) => t.stop());
        this.followRetries = 0;
      }
      return stream;
    } catch (e) {
      if (!current()) return null;
      const error = e instanceof VoiceError ? e : errorFromGum(e);
      if (swap && this.stream?.getAudioTracks()[0]?.readyState === "live") {
        console.warn("[voice] kept the current microphone:", error.message);
        // A Bluetooth mic may still be negotiating its profile: try again, keeping this one.
        this.retryFollow();
        return null;
      }
      this.releaseMic();
      this.micMissing = error.kind === "no_mic" || error.kind === "mic_failed";
      this.emit("micError", error);
      return null;
    }
  }

  private releaseMic(): void {
    this.inputGate?.close();
    this.inputGate = null;
    this.openingStream?.getTracks().forEach(track => track.stop());
    this.openingStream = null;
    this.handoverGate?.close();
    this.handoverGate = null;
    this.stopMeter();
    this.stream?.getTracks().forEach((t) => t.stop());
    this.stream = null;
    this.handover?.getTracks().forEach((t) => t.stop());
    this.handover = null;
  }

  private closeMic(): void {
    this.micGeneration++;
    this.releaseMic();
  }

  async retryMic(): Promise<void> {
    const stream = await this.openMic();
    if (!stream || this.stream !== stream) return;
    const track = stream.getAudioTracks()[0] ?? null;
    if (track) await this.updateSentTrack();
    this.applyTrackEnabled();
  }

  private async reopenMic(): Promise<void> {
    if (!this.stream && !this.pc) return;
    await this.retryMic();
  }

  // ---------------------------------------------------------- device changes

  private scheduleFollow(): void {
    if (this.followTimer) clearTimeout(this.followTimer);
    this.followTimer = setTimeout(() => {
      this.followTimer = null;
      void this.queueFollow(() => this.followDevices());
    }, FOLLOW_SETTLE_MS);
  }

  private queueFollow(step: () => Promise<void>): Promise<void> {
    const run = this.runId;
    this.following = this.following
      .then(() => (run === this.runId ? step() : undefined))
      .catch((e: unknown) => console.warn("[voice] device change failed:", e));
    return this.following;
  }

  /** 1, 2, 4, 8 s, then the next device event. */
  private retryFollow(): void {
    if (this.followRetryTimer || this.followRetries >= 4) return;
    const delay = 1000 * 2 ** this.followRetries++;
    this.followRetryTimer = setTimeout(() => {
      this.followRetryTimer = null;
      void this.queueFollow(() => this.followDevices());
    }, delay);
  }

  /**
   * After a device change: "System default" follows the browser's default, a chosen device that
   * went away falls back to the default and comes back with it, without renegotiating.
   */
  private async followDevices(): Promise<void> {
    if (!navigator.mediaDevices?.enumerateDevices) return;
    const all = await navigator.mediaDevices.enumerateDevices();
    this.emit("devices", deviceList(all));
    await this.followInput(all);
    this.followOutput(all);
  }

  private async followInput(all: MediaDeviceInfo[]): Promise<void> {
    const stream = this.stream;
    const inputs = all.filter((d) => realDevice(d, "audioinput"));
    if (!stream) {
      if (this.micMissing && inputs.length > 0) await this.swapMic(null);
      return;
    }
    const settings = trackSettings(stream);
    const wanted = this.inputDeviceId;
    if (wanted) {
      const present = inputs.some((d) => d.deviceId === wanted);
      if (present !== (settings.deviceId === wanted)) {
        await this.swapMic(stream);
        return;
      }
      if (present) return;
      // On the default while the chosen device is away: follow it like "System default".
    }
    const target = defaultDevice(all, "audioinput");
    if (!target) return; // the track's own "ended" reports a removal
    const same = settings.groupId && target.groupId ? settings.groupId === target.groupId : settings.deviceId === target.deviceId || settings.deviceId === "default";
    if (!same) await this.swapMic(stream);
  }

  /** Moves the microphone to where it belongs now, telling the user when it left or rejoined their choice. */
  private async swapMic(old: MediaStream | null): Promise<void> {
    if (this.stream !== old || !this.inCall()) return;
    const wanted = this.inputDeviceId;
    const wasOnWanted = trackSettings(old).deviceId === wanted;
    const stream = await this.openMic({ swap: true });
    if (!stream || this.stream !== stream) return;
    if (!wanted || !old || wasOnWanted === (trackSettings(stream).deviceId === wanted)) return;
    const label = await this.inputLabel(stream);
    if (label && this.stream === stream) this.emit("deviceSwitched", { side: "input", label });
  }

  private async inputLabel(stream: MediaStream): Promise<string> {
    const settings = trackSettings(stream);
    const all = await navigator.mediaDevices.enumerateDevices().catch(() => [] as MediaDeviceInfo[]);
    const entry =
      all.find((d) => realDevice(d, "audioinput") && d.deviceId === settings.deviceId) ??
      all.find((d) => realDevice(d, "audioinput") && !!settings.groupId && d.groupId === settings.groupId);
    return entry?.label || stream.getAudioTracks()[0]?.label || "";
  }

  /** A chosen output that went away falls back to the default, and returns; browsers move the default themselves. */
  private followOutput(all: MediaDeviceInfo[]): void {
    const wanted = this.outputDeviceId;
    const outputs = all.filter((d) => realDevice(d, "audiooutput"));
    // Without permission outputs are not listed: nothing is known about them.
    if (!wanted || outputs.length === 0 || !this.ctx) return;
    const chosen = outputs.find((d) => d.deviceId === wanted);
    const target = chosen ? wanted : null;
    if (target === this.routedOutputId) return;
    this.routeOutput(target);
    // Outside a call nothing plays: the move is not worth a word.
    const label = (chosen ?? defaultDevice(all, "audiooutput"))?.label;
    if (label && this.inCall()) this.emit("deviceSwitched", { side: "output", label });
  }

  private applyTrackEnabled(): void {
    const track = this.stream?.getAudioTracks()[0];
    if (!track) return;
    const unsupportedPtt = this.gateSupported === false && this.mode === "ptt";
    const blocked = this.muted || this.deafened || unsupportedPtt;
    // Keep capture running below the VAD threshold so it can open the gate again.
    const enabled = !blocked && (this.mode === "vad" || this.ptt);
    track.enabled = enabled;
    this.handover?.getAudioTracks().forEach((t) => { t.enabled = enabled; });
    const control = this.gateControl;
    const desired = this.mode === "ptt" || !this.automaticSensitivity;
    if (control && control.desired !== desired) {
      control.desired = desired;
      control.ready = false;
      const serial = ++control.serial;
      control.queue = control.queue.then(async () => {
        if (this.gateControl !== control || control.serial !== serial) return;
        await control.apply(desired);
        if (this.gateControl === control && control.serial === serial) {
          control.ready = true;
          this.applyTrackEnabled();
        }
      }).catch(error => {
        if (this.gateControl !== control) return;
        console.warn("[voice] input gate control failed:", error);
        this.teardownPeer();
        this.setState("failed");
      });
    }
    const controls = { threshold: this.threshold, ptt: this.mode === "ptt" ? this.ptt : null, blocked: blocked || (this.gateSupported === null && this.inCall()) || (control !== null && !control.ready), automatic: this.automaticSensitivity || this.gateSupported === false };
    this.inputGate?.configure(controls);
    this.handoverGate?.configure(controls);
    if (this.denoise) {
      this.denoise.track.enabled = enabled;
      this.denoise.setActive(enabled);
    }
  }

  // ----------------------------------------------------- noise suppression

  /** The microphone as sent: through the model while it runs, else as captured. */
  private sentTrack(): MediaStreamTrack | null {
    const captured = this.stream?.getAudioTracks()[0] ?? null;
    if (!captured || !this.inputGate) return null;
    const processed = this.denoise?.running ? this.denoise.track : captured;
    this.inputGate.setAudio(processed);
    if (this.meterTrack !== processed) this.startMeter(new MediaStream([processed]));
    return this.inputGate.track;
  }

  /** Sends `sentTrack()`, after any replacement still in progress, as it is when that is done. */
  private updateSentTrack(): Promise<void> {
    const sender = this.micSender;
    if (!sender) { this.sentTrack(); return Promise.resolve(); }
    const update = this.sentTrackUpdates.then(async () => {
      const track = this.sentTrack();
      if (this.micSender === sender && track && sender.track !== track) await sender.replaceTrack(track).catch(() => {});
    });
    this.sentTrackUpdates = update;
    return update;
  }

  private inCall(): boolean {
    return this.state === "starting" || this.state === "connected";
  }

  /** Loads the model in the background for this run; the browser's suppression works meanwhile. */
  private loadDenoise(): void {
    if (this.denoise || this.denoiseUnavailable || !this.capture.noiseSuppression || this.denoiseLoad?.run === this.runId) return;
    this.cancelDenoiseLoad();
    const load = { run: this.runId, abort: new AbortController() };
    this.denoiseLoad = load;
    let pipeline: DenoisePipeline | null = null;
    startDenoise(
      {
        trouble: reason => {
          if (pipeline && this.denoise === pipeline) this.denoiseTrouble(reason);
        },
        changed: () => {
          if (pipeline && this.denoise === pipeline) void this.updateSentTrack();
        },
      },
      load.abort.signal,
    ).then(
      async loaded => {
        // A load another run started, or one cancelled: its pipeline is not this run's.
        if (this.denoiseLoad !== load || load.run !== this.runId || !this.inCall()) {
          loaded.close();
          return;
        }
        this.denoiseLoad = null;
        pipeline = loaded;
        this.denoise = loaded;
        loaded.setStrength(this.capture.noiseStrength);
        // Captured again, without the browser's suppression, and sent through the model.
        await this.reopenMic();
      },
      (error: unknown) => {
        if (this.denoiseLoad !== load || load.run !== this.runId) return;
        this.denoiseLoad = null;
        this.denoiseTrouble(error instanceof Error ? error.message : String(error));
      },
    );
  }

  private cancelDenoiseLoad(): void {
    this.denoiseLoad?.abort.abort();
    this.denoiseLoad = null;
  }

  /** The model cannot run here: the browser's suppression for the rest of the page. */
  private denoiseTrouble(reason: string): void {
    console.warn("Noise suppression falls back to the browser's:", reason);
    this.denoiseUnavailable = true;
    if (!this.denoise) return;
    this.closeDenoise();
    void this.reopenMic();
  }

  private closeDenoise(): void {
    this.cancelDenoiseLoad();
    this.denoise?.close();
    this.denoise = null;
  }

  // ------------------------------------------------------------------ meter

  private startMeter(stream: MediaStream): void {
    this.stopMeter();
    this.meterTrack = stream.getAudioTracks()[0] ?? null;
    const ctx = this.ensureContext();
    this.analyser = ctx.createAnalyser();
    this.analyser.fftSize = 1024;
    this.meterBuf = new Float32Array(new ArrayBuffer(this.analyser.fftSize * 4));
    this.meterSource = ctx.createMediaStreamSource(stream);
    this.meterSource.connect(this.analyser);
    this.meterTimer = setInterval(() => {
      if (!this.analyser || !this.meterBuf) return;
      this.analyser.getFloatTimeDomainData(this.meterBuf);
      let sum = 0;
      for (const v of this.meterBuf) sum += v * v;
      const target = rmsToLevel(Math.sqrt(sum / this.meterBuf.length));
      // Fast attack, slower release: readable without flicker.
      this.smoothed = target > this.smoothed ? target : this.smoothed * 0.8 + target * 0.2;
      this.emit("level", this.smoothed < 0.01 ? 0 : this.smoothed);
    }, 50);
  }

  private stopMeter(): void {
    if (this.meterTimer) clearInterval(this.meterTimer);
    this.meterTimer = null;
    this.meterSource?.disconnect();
    this.meterSource = null;
    this.meterTrack = null;
    this.analyser = null;
    this.smoothed = 0;
    this.emit("level", 0);
  }

  async micPermission(): Promise<import("./engine").MicPermission> {
    try { return (await navigator.permissions.query({ name: "microphone" as PermissionName })).state; }
    catch { return "unknown"; }
  }

  startMicTest(onLevel: (level: number) => void, signal?: AbortSignal): Promise<() => void> {
    return this.micTests.start(async aborted => {
      if (this.state !== "idle" || this.muted || this.deafened || aborted.aborted) return () => {};
      return this.openMicTest(onLevel, aborted);
    }, signal);
  }

  private async openMicTest(onLevel: (level: number) => void, signal: AbortSignal): Promise<() => void> {
    const stream = await this.getStream(() => !signal.aborted);
    if (signal.aborted) { stream.getTracks().forEach(track => track.stop()); return () => {}; }
    let ctx: AudioContext;
    try { ctx = new AudioContext(); }
    catch (error) { stream.getTracks().forEach(track => track.stop()); throw error; }
    try {
      void ctx.resume().catch(() => {});
      let source = ctx.createMediaStreamSource(stream);
      let pipeline: DenoisePipeline | null = null;
      const load = new AbortController();
      const analyser = ctx.createAnalyser();
      analyser.fftSize = 1024;
      source.connect(analyser);
      const buf = new Float32Array(new ArrayBuffer(analyser.fftSize * 4));
      let smooth = 0;
      const timer = setInterval(() => {
        analyser.getFloatTimeDomainData(buf);
        let sum = 0;
        for (const v of buf) sum += v * v;
        const target = rmsToLevel(Math.sqrt(sum / buf.length));
        smooth = target > smooth ? target : smooth * 0.8 + target * 0.2;
        onLevel(smooth < 0.01 ? 0 : smooth);
      }, 50);
      void this.listDevices().then((d) => this.emit("devices", d));
      let stopped = false;
      const retarget = () => {
        if (stopped) return;
        source.disconnect();
        source = ctx.createMediaStreamSource(pipeline?.running ? new MediaStream([pipeline.track]) : stream);
        source.connect(analyser);
      };
      if (this.capture.noiseSuppression && !this.denoiseUnavailable) {
        void startDenoise({ changed: retarget, trouble: () => {
          pipeline?.close();
          if (this.previewDenoise === pipeline) this.previewDenoise = null;
          pipeline = null;
          if (!stopped) { void stream.getAudioTracks()[0]?.applyConstraints?.({ noiseSuppression: true }); retarget(); }
        } }, load.signal).then(async loaded => {
          if (stopped) { loaded.close(); return; }
          pipeline = loaded;
          this.previewDenoise = loaded;
          loaded.setStrength(this.capture.noiseStrength);
          loaded.setSource(stream);
          loaded.setActive(true);
          await stream.getAudioTracks()[0]?.applyConstraints?.({ noiseSuppression: false });
          if (!stopped) retarget();
        }).catch(() => {});
      }
      const stop = () => {
        if (stopped) return;
        stopped = true;
        clearInterval(timer);
        load.abort();
        pipeline?.close();
        if (this.previewDenoise === pipeline) this.previewDenoise = null;
        source.disconnect();
        stream.getTracks().forEach((t) => t.stop());
        void ctx.close();
        onLevel(0);
      };
      signal.addEventListener("abort", stop, { once: true });
      return () => { signal.removeEventListener("abort", stop); stop(); };
    } catch (error) {
      stream.getTracks().forEach(track => track.stop());
      void ctx.close();
      throw error;
    }
  }

  // ------------------------------------------------------------------ output

  private ensureContext(): SinkAudioContext {
    if (this.ctx) return this.ctx;
    const ctx: SinkAudioContext = new AudioContext({ latencyHint: "interactive" });
    this.ctx = ctx;
    this.master = ctx.createGain();
    this.applyMasterGain();
    this.routeOutput();
    if (!this.resumeBound) {
      this.resumeBound = true;
      // Autoplay policy: a context created outside a gesture stays suspended.
      const resume = () => {
        if (ctx.state !== "running") void ctx.resume();
      };
      for (const type of ["pointerdown", "keydown"]) window.addEventListener(type, resume, { passive: true });
    }
    return ctx;
  }

  private applyMasterGain(): void {
    if (!this.master || !this.ctx) return;
    const target = this.deafened ? 0 : this.masterVolume;
    this.master.gain.setTargetAtTime(target, this.ctx.currentTime, 0.015);
  }

  /** Connects master -> device. Uses AudioContext.setSinkId when available, else an <audio> element. */
  private routeOutput(id = this.outputDeviceId): void {
    const ctx = this.ctx;
    const master = this.master;
    if (!ctx || !master) return;
    master.disconnect();
    this.routedOutputId = id;
    const attempt = ++this.routeAttempt;
    const ctxSink = typeof ctx.setSinkId === "function";
    const elementSink = typeof HTMLMediaElement !== "undefined" && "setSinkId" in HTMLMediaElement.prototype;
    // A device that cannot be routed to plays on the default and counts as not routed, so its
    // return routes it again.
    const unrouted = () => {
      if (attempt !== this.routeAttempt || !id) return;
      this.routedOutputId = null;
      master.disconnect();
      master.connect(ctx.destination);
      this.sinkElement?.pause();
      void ctx.setSinkId?.("").catch(() => {});
    };
    if (!id || ctxSink) {
      master.connect(ctx.destination);
      this.sinkElement?.pause();
      void ctx.setSinkId?.(id ?? "").catch(unrouted);
      return;
    }
    if (elementSink) {
      this.streamDest ??= ctx.createMediaStreamDestination();
      master.connect(this.streamDest);
      if (!this.sinkElement) {
        this.sinkElement = new Audio() as SinkAudioElement;
        this.sinkElement.srcObject = this.streamDest.stream;
      }
      void this.sinkElement.setSinkId?.(id).catch(unrouted);
      void this.sinkElement.play().catch(() => {});
      return;
    }
    master.connect(ctx.destination);
  }

  private attachSlot(slot: number, track: MediaStreamTrack): void {
    const ctx = this.ensureContext();
    this.detachSlot(slot);
    const stream = new MediaStream([track]);
    // Chromium only feeds remote WebRTC audio into WebAudio when the stream is
    // also attached to a media element; keep a muted one around.
    const element = new Audio();
    element.srcObject = stream;
    element.muted = true;
    void element.play().catch(() => {});
    const source = ctx.createMediaStreamSource(stream);
    const gain = ctx.createGain();
    source.connect(gain);
    if (this.master) gain.connect(this.master);
    const owner = this.slotOwners[slot] ?? null;
    this.pipes[slot] = { source, gain, element, owner };
    this.applySlotGain(slot);
  }

  private detachSlot(slot: number): void {
    const pipe = this.pipes[slot];
    if (!pipe) return;
    pipe.source.disconnect();
    pipe.gain.disconnect();
    pipe.element.srcObject = null;
    this.pipes[slot] = null;
  }

  private applySlotGain(slot: number): void {
    const pipe = this.pipes[slot];
    if (!pipe || !this.ctx) return;
    const owner = this.slotOwners[slot] ?? null;
    const volume = owner ? (this.userVolumes.get(owner) ?? 1) : 1;
    pipe.gain.gain.setTargetAtTime(volume, this.ctx.currentTime, 0.015);
  }

  // ----------------------------------------------------------------- setters

  setMuted(muted: boolean): void {
    if (muted) this.cancelPttRelease();
    this.muted = muted;
    if (muted) void this.micTests.stop();
    this.applyTrackEnabled();
    if (muted) this.closeMic();
    else if (!this.deafened && this.inCall()) void this.retryMic();
  }

  setDeafened(deafened: boolean): void {
    if (deafened) this.cancelPttRelease();
    this.deafened = deafened;
    if (deafened) void this.micTests.stop();
    this.applyTrackEnabled();
    this.applyMasterGain();
    if (deafened) this.closeMic();
    else if (!this.muted && this.inCall()) void this.retryMic();
  }

  setInputMode(mode: InputMode): void {
    this.cancelPttRelease();
    this.mode = mode;
    this.applyTrackEnabled();
  }

  setInputSensitivity(threshold: number | null): void {
    this.automaticSensitivity = threshold === null;
    this.threshold = inputThreshold(threshold);
    this.applyTrackEnabled();
  }

  setPttActive(active: boolean): void {
    if (active) {
      if (this.pttRelease) clearTimeout(this.pttRelease);
      this.pttRelease = null;
      this.ptt = true;
      this.applyTrackEnabled();
    } else if (this.ptt && !this.pttRelease) {
      // Match native's 300 ms hangover, including the denoiser's buffered word ending.
      this.pttRelease = setTimeout(() => {
        this.pttRelease = null;
        this.ptt = false;
        this.applyTrackEnabled();
      }, 300);
    }
  }

  private cancelPttRelease(): void {
    if (!this.pttRelease) return;
    clearTimeout(this.pttRelease);
    this.pttRelease = null;
    this.ptt = false;
  }

  async setInputDevice(deviceId: string | null): Promise<void> {
    if (this.inputDeviceId === deviceId) return;
    this.inputDeviceId = deviceId;
    this.followRetries = 0;
    await this.reopenMic();
  }

  async setCaptureOptions(options: CaptureOptions): Promise<void> {
    const changed =
      options.noiseSuppression !== this.capture.noiseSuppression ||
      options.echoCancellation !== this.capture.echoCancellation ||
      options.autoGainControl !== this.capture.autoGainControl;
    this.capture = options;
    this.denoise?.setStrength(options.noiseStrength);
    this.previewDenoise?.setStrength(options.noiseStrength);
    if (!options.noiseSuppression) this.closeDenoise();
    if (changed) await this.reopenMic();
    if (this.inCall()) this.loadDenoise();
  }

  async setOutputDevice(deviceId: string | null): Promise<void> {
    this.outputDeviceId = deviceId;
    this.routeOutput();
  }

  setMasterVolume(volume: number): void {
    this.masterVolume = Math.min(1.5, Math.max(0, volume));
    this.applyMasterGain();
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
    const pipe = this.pipes[slot];
    if (pipe) pipe.owner = uid;
    this.applySlotGain(slot);
  }

  async cryptoCounters(): Promise<Record<string, number> | null> {
    return (await this.sframe?.counters()) ?? null;
  }

  async listDevices(): Promise<DeviceList> {
    if (!navigator.mediaDevices?.enumerateDevices) return { inputs: [], outputs: [] };
    return deviceList(await navigator.mediaDevices.enumerateDevices());
  }
}
