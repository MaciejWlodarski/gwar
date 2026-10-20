import { startDenoise, type DenoisePipeline } from "./denoise";
import { nextMediaRun } from "./engine";
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
  private sframe: SframeWorker | null = null;
  private micSender: RTCRtpSender | null = null;
  private stream: MediaStream | null = null;
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
  private masterVolume = 1;
  private inputDeviceId: string | null = null;
  private outputDeviceId: string | null = null;
  private capture: CaptureOptions = { noiseSuppression: true, noiseStrength: 100, echoCancellation: true, autoGainControl: true };
  /**
   * Gwar's own noise suppression (./denoise) once it is loaded; the browser's until then, and for
   * the rest of the page if it cannot run here.
   */
  private denoise: DenoisePipeline | null = null;
  /** The model loading for a run (`runId`), cancelled when that run ends or suppression goes off. */
  private denoiseLoad: { run: number; abort: AbortController } | null = null;
  private denoiseUnavailable = false;
  /** Replacements of the sent track, one at a time: `sender.track` changes only once one completes. */
  private sentTrackUpdates: Promise<void> = Promise.resolve();
  private state: VoiceState = "idle";
  private runId = 0;
  private micGeneration = 0;
  private resumeBound = false;

  constructor() {
    this.capabilities = {
      encryptedVoice: () => encryptedVoiceSupported(),
      outputDeviceSelection:
        typeof AudioContext !== "undefined" &&
        ("setSinkId" in AudioContext.prototype || (typeof HTMLMediaElement !== "undefined" && "setSinkId" in HTMLMediaElement.prototype)),
    };
    if (typeof navigator !== "undefined" && navigator.mediaDevices?.addEventListener) {
      navigator.mediaDevices.addEventListener("devicechange", () => {
        void this.listDevices().then((d) => this.emit("devices", d));
      });
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

  async start({ offer, iceServers, mode, binding, keys }: Parameters<VoiceEngine["start"]>[0]): Promise<void> {
    const encrypted = mode === "encrypted";
    const { server_key, call, participation, group } = binding;
    if (encrypted && (!keys || !server_key || !call || !participation || !group)) throw new VoiceError("unsupported", "encrypted voice needs its call binding and keys");
    const mediaRun = nextMediaRun();
    const run = ++this.runId;
    // A model still loading is the run before's.
    this.cancelDenoiseLoad();
    // Closes the old gate and drops its queued audio before the new run starts.
    this.teardownPeer();
    if (encrypted && !(await encryptedVoiceSupported())) {
      this.setState("failed");
      throw new VoiceError("unsupported", "Update your browser or use the desktop app to join encrypted voice.");
    }
    if (run !== this.runId) return;
    this.setState("starting");
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
      const answer = await offer(pc.localDescription?.sdp ?? "", mediaRun);
      if (run !== this.runId) return;
      await pc.setRemoteDescription({ type: "answer", sdp: answer });
      if (run !== this.runId) return;
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
    this.teardownPeer();
    this.closeMic();
    this.closeDenoise();
    this.setState("idle");
  }

  private teardownPeer(): void {
    // Invalidate the run's keys first, then the transport.
    this.sframe?.stop();
    this.sframe = null;
    const pc = this.pc;
    this.pc = null;
    this.micSender = null;
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

  private async openMic(): Promise<MediaStream | null> {
    this.closeMic();
    const generation = this.micGeneration;
    const run = this.runId;
    const current = () => generation === this.micGeneration && run === this.runId;
    try {
      const stream = await this.getStream(current);
      if (!current()) {
        stream.getTracks().forEach((track) => track.stop());
        return null;
      }
      this.stream = stream;
      this.denoise?.setSource(stream);
      this.startMeter(stream);
      this.emit("micError", null);
      this.applyTrackEnabled();
      const track = stream.getAudioTracks()[0];
      track?.addEventListener("ended", () => {
        if (this.stream === stream) {
          this.closeMic();
          this.emit("micError", new VoiceError("no_mic", "input device disconnected"));
        }
      });
      // Labels become available only after permission was granted.
      void this.listDevices().then((d) => { if (current()) this.emit("devices", d); });
      return stream;
    } catch (e) {
      if (current()) this.emit("micError", e instanceof VoiceError ? e : errorFromGum(e));
      return null;
    }
  }

  private closeMic(): void {
    this.micGeneration++;
    this.stopMeter();
    this.stream?.getTracks().forEach((t) => t.stop());
    this.stream = null;
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

  private applyTrackEnabled(): void {
    const track = this.stream?.getAudioTracks()[0];
    if (!track) return;
    const enabled = !this.muted && !this.deafened && (this.mode === "vad" || this.ptt);
    track.enabled = enabled;
    if (this.denoise) {
      this.denoise.track.enabled = enabled;
      this.denoise.setActive(enabled);
    }
  }

  // ----------------------------------------------------- noise suppression

  /** The microphone as sent: through the model while it runs, else as captured. */
  private sentTrack(): MediaStreamTrack | null {
    const captured = this.stream?.getAudioTracks()[0] ?? null;
    return captured && this.denoise?.running ? this.denoise.track : captured;
  }

  /** Sends `sentTrack()`, after any replacement still in progress, as it is when that is done. */
  private updateSentTrack(): Promise<void> {
    const update = this.sentTrackUpdates.then(async () => {
      const sender = this.micSender, track = this.sentTrack();
      if (track && sender && sender.track !== track) await sender.replaceTrack(track).catch(() => {});
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
    this.analyser = null;
    this.smoothed = 0;
    this.emit("level", 0);
  }

  async startMicTest(onLevel: (level: number) => void): Promise<() => void> {
    const stream = await this.getStream();
    const ctx = new AudioContext();
    const source = ctx.createMediaStreamSource(stream);
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
    return () => {
      clearInterval(timer);
      stream.getTracks().forEach((t) => t.stop());
      void ctx.close();
      onLevel(0);
    };
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
  private routeOutput(): void {
    const ctx = this.ctx;
    const master = this.master;
    if (!ctx || !master) return;
    master.disconnect();
    const id = this.outputDeviceId;
    const ctxSink = typeof ctx.setSinkId === "function";
    const elementSink = typeof HTMLMediaElement !== "undefined" && "setSinkId" in HTMLMediaElement.prototype;
    if (!id || ctxSink) {
      master.connect(ctx.destination);
      this.sinkElement?.pause();
      void ctx.setSinkId?.(id ?? "").catch(() => {});
      return;
    }
    if (elementSink) {
      this.streamDest ??= ctx.createMediaStreamDestination();
      master.connect(this.streamDest);
      if (!this.sinkElement) {
        this.sinkElement = new Audio() as SinkAudioElement;
        this.sinkElement.srcObject = this.streamDest.stream;
      }
      void this.sinkElement.setSinkId?.(id).catch(() => {});
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
    this.muted = muted;
    this.applyTrackEnabled();
  }

  setDeafened(deafened: boolean): void {
    this.deafened = deafened;
    this.applyTrackEnabled();
    this.applyMasterGain();
  }

  setInputMode(mode: InputMode): void {
    this.mode = mode;
    this.applyTrackEnabled();
  }

  setPttActive(active: boolean): void {
    this.ptt = active;
    this.applyTrackEnabled();
  }

  async setInputDevice(deviceId: string | null): Promise<void> {
    if (this.inputDeviceId === deviceId) return;
    this.inputDeviceId = deviceId;
    await this.reopenMic();
  }

  async setCaptureOptions(options: CaptureOptions): Promise<void> {
    const changed =
      options.noiseSuppression !== this.capture.noiseSuppression ||
      options.echoCancellation !== this.capture.echoCancellation ||
      options.autoGainControl !== this.capture.autoGainControl;
    this.capture = options;
    this.denoise?.setStrength(options.noiseStrength);
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
    const all = await navigator.mediaDevices.enumerateDevices();
    const map = (kind: MediaDeviceKind, fallback: string): AudioDevice[] =>
      all
        .filter((d) => d.kind === kind && d.deviceId !== "default" && d.deviceId !== "communications")
        .map((d, i) => ({ id: d.deviceId, label: d.label || `${fallback} ${i + 1}` }));
    return { inputs: map("audioinput", "Microphone"), outputs: map("audiooutput", "Speaker") };
  }
}
