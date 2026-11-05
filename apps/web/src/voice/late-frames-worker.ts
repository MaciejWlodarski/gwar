/** Runs `LateFrames` on a watch slot's received frames, where the receiver has not seen them yet. */
import { filterFrames, type EncodedFrame, type MediaKind } from "./late-frames";

interface Transformer { readonly readable: ReadableStream<EncodedFrame>; readonly writable: WritableStream<EncodedFrame>; readonly options: { slot: number; kind: MediaKind } }
export interface LinkBack { slot: number }

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope {
  onrtctransform: ((event: { transformer: Transformer }) => void) | null;
  postMessage(message: LinkBack): void;
}
const self = globalThis as unknown as WorkerScope;

self.onrtctransform = event => {
  const { readable, writable, options: { slot, kind } } = event.transformer;
  // Only the picture's link-back asks for a keyframe and the estimate back.
  void filterFrames(readable, writable, kind, () => { if (kind === "video") self.postMessage({ slot }); });
};
