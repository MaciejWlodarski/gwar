import worklet from "./input-gate-worklet.ts?worker&url";
import type { GateControls } from "./input-gate-worklet";

export interface CaptureGate {
  readonly track: MediaStreamTrack;
  setAudio(track: MediaStreamTrack): void;
  configure(controls: GateControls): void;
  close(): void;
}

const loaded = new WeakMap<AudioContext, Promise<void>>();

/** Only this worklet's output can reach WebRTC. A failed/suspended worklet sends silence. */
export async function startInputGate(context: AudioContext, stream: MediaStream): Promise<CaptureGate> {
  let ready = loaded.get(context);
  if (!ready) {
    ready = context.audioWorklet.addModule(worklet).catch(error => {
      loaded.delete(context); // A network failure must not make Retry require a page reload.
      throw error;
    });
    loaded.set(context, ready);
  }
  await ready;
  const node = new AudioWorkletNode(context, "gwar-input-gate", {
    numberOfInputs: 2, numberOfOutputs: 1, outputChannelCount: [1], channelCount: 1, channelCountMode: "explicit",
  });
  const destination = context.createMediaStreamDestination();
  const track = destination.stream.getAudioTracks()[0]!;
  // Closed before being attached to a sender, including while controls are queued.
  track.enabled = false;
  node.connect(destination);
  const detector = context.createMediaStreamSource(stream);
  detector.connect(node, 0, 0);
  let source: MediaStreamAudioSourceNode | null = null;
  let audio: MediaStreamTrack | null = null;
  return {
    track,
    setAudio(next) {
      if (audio === next) return;
      source?.disconnect();
      audio = next;
      source = context.createMediaStreamSource(new MediaStream([next]));
      source.connect(node, 0, 1);
    },
    configure(controls) {
      track.enabled = !controls.blocked && controls.ptt !== false;
      node.port.postMessage(controls);
    },
    close() {
      track.enabled = false;
      track.stop();
      detector.disconnect();
      source?.disconnect();
      node.disconnect();
      node.port.close();
    },
  };
}
