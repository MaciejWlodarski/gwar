/**
 * Hands received share sound to the native mixer. A frame is never written back to the
 * transform's output, so the webview has nothing to decode and cannot play share sound.
 */
interface EncodedAudioFrame { data: ArrayBuffer; timestamp?: number; getMetadata?(): { rtpTimestamp?: number } }
interface Transformer { readonly readable: ReadableStream<EncodedAudioFrame>; readonly writable: WritableStream; readonly options: { slot: number } }
export interface ShareSoundFrame { slot: number; rtpTime: number; data: ArrayBuffer }
export interface ShareSoundProbe { transformEvents: boolean }

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope {
  onmessage: (() => void) | null;
  onrtctransform: ((event: { transformer: Transformer }) => void) | null;
  postMessage(message: ShareSoundFrame | ShareSoundProbe, transfer?: Transferable[]): void;
}
const self = globalThis as unknown as WorkerScope;
/** Opus never needs more per packet; anything larger is not a frame worth copying. */
const MAX_FRAME = 1_275;

async function drain(transformer: Transformer): Promise<void> {
  const { slot } = transformer.options;
  const reader = transformer.readable.getReader();
  for (;;) {
    const { value: frame, done } = await reader.read();
    if (done) return;
    const rtpTime = frame.getMetadata?.().rtpTimestamp ?? frame.timestamp;
    if (rtpTime === undefined || frame.data.byteLength > MAX_FRAME) continue;
    const data = frame.data.slice(0);
    self.postMessage({ slot, rtpTime, data }, [data]);
  }
}

self.onmessage = () => self.postMessage({ transformEvents: "onrtctransform" in self });
self.onrtctransform = event => void drain(event.transformer).catch(() => {});
