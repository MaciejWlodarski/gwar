/**
 * The model is built in a worker (`build-worker.ts`) and handed to the audio thread as a copy of
 * the module's memory. Building it takes ~0.3 s, and 5 to 8 s on a saturated machine: on the
 * audio thread that held up every render of the capture context, and in Chromium the page with
 * it, since its main thread takes a lock the audio device holds through a render. A copy of the
 * memory is the same model: the module keeps all its state there (`crates/gwar-denoise-wasm`
 * has no other mutable global than the stack pointer, back at its start between calls).
 */

/** The module's exports for its errors; the worklet's `Exports` has the rest. */
export interface ErrorExports {
  memory: WebAssembly.Memory;
  gwar_denoise_error(): number;
  gwar_denoise_error_len(): number;
}

/** The module's exports for building the model. */
export interface ModelExports extends ErrorExports {
  gwar_denoise_archive(length: number): number;
  gwar_denoise_new(): number;
}

/**
 * Bytes of memory the audio thread copies at a time, each in a task of its own, so that a render
 * never waits long for the thread.
 */
export const CHUNK = 2 << 20;
const PAGE = 65_536;

/** The last call's error message; ASCII (the audio thread has no TextDecoder). */
export function modelError(exports: ErrorExports): string {
  const bytes = new Uint8Array(exports.memory.buffer, exports.gwar_denoise_error(), exports.gwar_denoise_error_len());
  return String.fromCharCode(...bytes);
}

/** Builds the model from `archive`; the denoiser's address. */
export function buildModel(exports: ModelExports, archive: Uint8Array): number {
  const at = exports.gwar_denoise_archive(archive.length);
  new Uint8Array(exports.memory.buffer, at, archive.length).set(archive);
  const denoiser = exports.gwar_denoise_new();
  if (!denoiser) throw new Error(modelError(exports));
  return denoiser;
}

/** Copies of `memory` in `CHUNK`s, from the start. */
export function* memoryChunks(memory: WebAssembly.Memory): Generator<{ offset: number; bytes: ArrayBuffer }> {
  const { buffer } = memory;
  for (let offset = 0; offset < buffer.byteLength; offset += CHUNK) yield { offset, bytes: buffer.slice(offset, offset + CHUNK) };
}

/** Grows a fresh instance's memory to `size` bytes, for the copy. */
export function growMemory(memory: WebAssembly.Memory, size: number): void {
  if (size % PAGE) throw new Error(`memory of ${size} bytes`);
  const missing = (size - memory.buffer.byteLength) / PAGE;
  if (missing < 0) throw new Error("memory larger than the copy");
  if (missing) memory.grow(missing);
}

/** Writes one chunk of the copy. */
export function writeChunk(memory: WebAssembly.Memory, offset: number, bytes: ArrayBuffer): void {
  new Uint8Array(memory.buffer, offset, bytes.byteLength).set(new Uint8Array(bytes));
}
