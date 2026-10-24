/**
 * Debug builds of the desktop app only (`GWAR_CAPTURE_CHECK=1`): checks that
 * the webview can capture the screen and that it cannot open the microphone
 * or camera (docs/screen-share.md, PR 1). The capture is encoded through a
 * loopback peer connection, so the frame rate is the one an encoder would
 * get, also while the window is hidden. Results go to the page and the Rust
 * log.
 */
import { useEffect, useRef, useState } from "react";
import { Button, Select } from "../ui/kit";
import { tauri } from ".";
import { preferredCodecs } from "../voice/screen";

interface Sample {
  /** Seconds since the capture started. */
  at: number;
  visible: boolean;
  /** Frames per second the capture delivered (`media-source`), when the engine reports it. */
  source: number | null;
  /** Frames per second encoded, from `framesEncoded`. */
  encoded: number;
}

/**
 * One click's capture, claimed before `getDisplayMedia` resolves so a second click cannot start
 * another. Whatever arrives after it was stopped is released at once.
 */
interface Capture {
  stream: MediaStream | null;
  peers: RTCPeerConnection[];
  timer: ReturnType<typeof setInterval> | null;
  stopped: boolean;
}

/** Chromium's `CaptureController` (not in the DOM typings yet). */
interface CaptureControllerLike {
  setFocusBehavior(behavior: "focus-captured-surface" | "no-focus-change"): void;
}
type CaptureControllerConstructor = new () => CaptureControllerLike;

function log(message: string): void {
  void tauri().then((api) => api.invoke("debug_log", { message: `capture-check: ${message}` })).catch(() => {});
}

function describeError(e: unknown): string {
  return e instanceof Error ? `${e.name}: ${e.message}` : String(e);
}

/** Tries one `getUserMedia` request; the desktop must deny it. */
async function probeUserMedia(constraints: MediaStreamConstraints): Promise<string> {
  if (!navigator.mediaDevices?.getUserMedia) return "no getUserMedia";
  try {
    const stream = await navigator.mediaDevices.getUserMedia(constraints);
    for (const track of stream.getTracks()) track.stop();
    return "GRANTED (expected a denial)";
  } catch (e) {
    return `denied (${describeError(e)})`;
  }
}

function summarize(samples: Sample[], visible: boolean): string {
  const rates = samples.filter((s) => s.visible === visible).map((s) => s.encoded).sort((a, b) => a - b);
  if (rates.length === 0) return "no samples";
  const mean = rates.reduce((sum, r) => sum + r, 0) / rates.length;
  const median = rates[Math.floor(rates.length / 2)] ?? 0;
  return `${rates.length} s, encoded fps min ${rates[0]?.toFixed(1)}, median ${median.toFixed(1)}, mean ${mean.toFixed(1)}`;
}

/** Uses the production screen publisher codec policy. */
function preferScreenCodecs(transceiver: RTCRtpTransceiver): void {
  const codecs = RTCRtpSender.getCapabilities?.("video")?.codecs;
  if (!codecs || !transceiver.setCodecPreferences) return;
  transceiver.setCodecPreferences(preferredCodecs(codecs));
}

/** Sends the track from one peer connection to another on this page. */
async function loopback(track: MediaStreamTrack): Promise<RTCPeerConnection[]> {
  const send = new RTCPeerConnection();
  const receive = new RTCPeerConnection();
  try {
    await connect(send, receive, track);
  } catch (e) {
    send.close();
    receive.close();
    throw e;
  }
  return [send, receive];
}

async function connect(send: RTCPeerConnection, receive: RTCPeerConnection, track: MediaStreamTrack): Promise<void> {
  send.onicecandidate = (e) => void (e.candidate && receive.addIceCandidate(e.candidate).catch(() => {}));
  receive.onicecandidate = (e) => void (e.candidate && send.addIceCandidate(e.candidate).catch(() => {}));
  const transceiver = send.addTransceiver(track, { direction: "sendonly" });
  preferScreenCodecs(transceiver);
  const parameters = transceiver.sender.getParameters();
  for (const encoding of parameters.encodings ?? []) encoding.maxBitrate = 4_000_000;
  await transceiver.sender.setParameters(parameters).catch(() => {});
  await send.setLocalDescription();
  await receive.setRemoteDescription(send.localDescription!);
  await receive.setLocalDescription();
  await send.setRemoteDescription(receive.localDescription!);
}

function environment(): string[] {
  const has = (name: string, value: unknown) => `${name}: ${value ? "yes" : "no"}`;
  return [
    `user agent: ${navigator.userAgent}`,
    has("secure context", window.isSecureContext),
    has("getDisplayMedia", typeof navigator.mediaDevices?.getDisplayMedia === "function"),
    has("RTCPeerConnection", typeof RTCPeerConnection === "function"),
    has("RTCRtpScriptTransform", typeof RTCRtpScriptTransform === "function"),
    has("CaptureController", "CaptureController" in window),
  ];
}

/** Asks for the capture with the constraints shares will use (docs/screen-share.md, "Browsers"). */
function requestCapture(frameRate: number, audio: boolean) {
  const Controller = (window as { CaptureController?: CaptureControllerConstructor }).CaptureController;
  const controller = Controller ? new Controller() : undefined;
  const options = {
    video: { frameRate },
    audio,
    selfBrowserSurface: "exclude",
    surfaceSwitching: "include",
    systemAudio: audio ? "include" : "exclude",
    controller,
  } as DisplayMediaStreamOptions;
  const asked = performance.now();
  return { request: navigator.mediaDevices.getDisplayMedia(options), controller, asked };
}

export function CaptureCheck() {
  const [lines, setLines] = useState(environment);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [fps, setFps] = useState("30");
  const [audio, setAudio] = useState("off");
  const [capturing, setCapturing] = useState(false);
  const capture = useRef<Capture | null>(null);

  const report = (line: string) => {
    log(line);
    setLines((all) => [...all, line]);
  };

  useEffect(() => {
    for (const line of lines) log(line);
    void (async () => {
      report(`microphone: ${await probeUserMedia({ audio: true })}`);
      report(`camera: ${await probeUserMedia({ video: true })}`);
    })();
    return () => stop(capture);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // No await before getDisplayMedia: it needs the click's user activation.
  const start = () => {
    if (capture.current) return;
    const session: Capture = { stream: null, peers: [], timer: null, stopped: false };
    capture.current = session;
    setCapturing(true);
    const { request, controller, asked } = requestCapture(Number(fps), audio === "on");
    request.then(
      (stream) => {
        session.stream = stream;
        if (session.stopped) return release(session);
        void begin(session, stream, controller, asked);
      },
      (e: unknown) => {
        report(`getDisplayMedia failed after ${Math.round(performance.now() - asked)} ms: ${describeError(e)}`);
        if (capture.current === session) end();
      },
    );
  };

  const end = () => {
    stop(capture);
    setCapturing(false);
  };

  const begin = async (session: Capture, stream: MediaStream, controller: CaptureControllerLike | undefined, asked: number) => {
    try {
      controller?.setFocusBehavior("no-focus-change");
    } catch {
      // Only tabs and windows have a focus behavior.
    }
    report(`getDisplayMedia resolved after ${Math.round(performance.now() - asked)} ms`);
    const [video] = stream.getVideoTracks();
    if (!video) {
      report("no video track");
      if (capture.current === session) end();
      return;
    }
    video.contentHint = "detail";
    report(`video settings: ${JSON.stringify(video.getSettings())}`);
    for (const track of stream.getAudioTracks()) report(`audio track: ${track.label} ${JSON.stringify(track.getSettings())}`);
    if (stream.getAudioTracks().length === 0) report("audio tracks: none");
    video.addEventListener("ended", () => report("capture ended by the system or the user"));
    const peers = await loopback(video).catch((e: unknown) => {
      report(`loopback failed: ${describeError(e)}`);
      return [];
    });
    session.peers = peers;
    // Stopped while the peers connected: nothing more to measure.
    if (session.stopped) return release(session);
    const started = performance.now();
    let last = { at: started, encoded: 0 };
    const timer = setInterval(() => {
      void peers[0]?.getStats().then((stats) => {
        let source: number | null = null;
        let encoded = 0;
        let detail = "";
        stats.forEach((s: Record<string, unknown>) => {
          if (s.type === "media-source" && s.kind === "video" && typeof s.framesPerSecond === "number") source = s.framesPerSecond;
          if (s.type === "outbound-rtp" && s.kind === "video") {
            encoded = Number(s.framesEncoded ?? 0);
            detail = `${String(s.frameWidth)}x${String(s.frameHeight)}, ${String(s.encoderImplementation ?? "?")}, limited by ${String(s.qualityLimitationReason ?? "?")}`;
          }
        });
        const now = performance.now();
        const sample: Sample = {
          at: Math.round((now - started) / 1000),
          visible: document.visibilityState === "visible",
          source,
          encoded: ((encoded - last.encoded) * 1000) / (now - last.at),
        };
        last = { at: now, encoded };
        setSamples((all) => [...all, sample]);
        if (sample.at % 30 === 0) {
          log(`${sample.at} s ${sample.visible ? "visible" : "hidden"}: encoded ${sample.encoded.toFixed(1)} fps, source ${String(source)} fps, ${detail}`);
        }
      });
    }, 1000);
    session.timer = timer;
    setSamples([]);
  };

  const latest = samples.at(-1);
  const copy = () =>
    void navigator.clipboard.writeText(JSON.stringify({ lines, visible: summarize(samples, true), hidden: summarize(samples, false), samples }));

  return (
    <div className="flex h-dvh flex-col gap-4 overflow-auto bg-surface p-6 text-sm text-fg">
      <h1 className="text-lg font-semibold">Screen capture check (debug build)</h1>
      <div className="flex flex-wrap items-center gap-3">
        <Select
          value={fps}
          onValueChange={setFps}
          options={["15", "30", "60"].map((v) => ({ value: v, label: `${v} fps` }))}
          disabled={capturing}
        />
        <Select
          value={audio}
          onValueChange={setAudio}
          options={[{ value: "off", label: "No audio" }, { value: "on", label: "Ask for audio" }]}
          disabled={capturing}
        />
        <Button variant="primary" onClick={start} disabled={capturing}>Share screen</Button>
        <Button onClick={() => { end(); report("stopped"); }} disabled={!capturing}>Stop</Button>
        <Button onClick={copy}>Copy report</Button>
      </div>
      {latest && (
        <div className="font-mono">
          <div>now: encoded {latest.encoded.toFixed(1)} fps, source {latest.source ?? "?"} fps ({latest.visible ? "visible" : "hidden"})</div>
          <div>visible: {summarize(samples, true)}</div>
          <div>hidden: {summarize(samples, false)}</div>
        </div>
      )}
      <pre className="whitespace-pre-wrap break-all font-mono text-xs text-muted">{lines.join("\n")}</pre>
    </div>
  );
}

function stop(capture: { current: Capture | null }): void {
  const current = capture.current;
  capture.current = null;
  if (current) release(current);
}

/** Marks a capture stopped and frees what it holds so far; later arrivals free themselves. */
function release(session: Capture): void {
  session.stopped = true;
  if (session.timer !== null) clearInterval(session.timer);
  session.timer = null;
  for (const track of session.stream?.getTracks() ?? []) track.stop();
  for (const peer of session.peers) peer.close();
  session.peers = [];
}
