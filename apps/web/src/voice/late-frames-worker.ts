/** Runs `LateFrames` on a watch slot's received frames, where the receiver has not seen them yet. */
import { LateFrames, type MediaKind } from "./late-frames";

interface EncodedFrame { type?: string; timestamp?: number; getMetadata?(): { rtpTimestamp?: number } }
interface Transformer { readonly readable: ReadableStream<EncodedFrame>; readonly writable: WritableStream<EncodedFrame>; readonly options: { slot: number; kind: MediaKind } }
export interface LinkBack { slot: number }

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope {
  onrtctransform: ((event: { transformer: Transformer }) => void) | null;
  postMessage(message: LinkBack): void;
}
const self = globalThis as unknown as WorkerScope;

async function run(transformer: Transformer): Promise<void> {
  const { slot, kind } = transformer.options;
  const late = new LateFrames(kind);
  const reader = transformer.readable.getReader();
  const writer = transformer.writable.getWriter();
  for (;;) {
    const { value: frame, done } = await reader.read();
    if (done) return;
    const rtp = frame.getMetadata?.().rtpTimestamp ?? frame.timestamp;
    const verdict = rtp === undefined ? { keep: true, back: false } : late.see(rtp, performance.now(), frame.type === "key");
    if (verdict.keep) await writer.write(frame);
    if (verdict.back && kind === "video") self.postMessage({ slot });
  }
}

self.onrtctransform = event => void run(event.transformer).catch(() => {});
