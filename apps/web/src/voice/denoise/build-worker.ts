/**
 * Builds the noise suppression's model off the page and off the audio thread: downloads (or takes
 * from the Cache API) the module and the model's archive, checks the archive against its pin,
 * builds the model, and hands the module's memory to the worklet a chunk at a time (snapshot.ts).
 */
import { buildModel, memoryChunks, type ModelExports } from "./snapshot";
import type { Build, BuildFailed, Handover, Next } from "./protocol";

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope {
  onmessage: ((event: MessageEvent<Build>) => void) | null;
  postMessage(message: BuildFailed): void;
  close(): void;
}
const self = globalThis as unknown as WorkerScope;

async function sha256(bytes: ArrayBuffer): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return Array.from(digest, b => b.toString(16).padStart(2, "0")).join("");
}

/** A same-origin asset, from the Cache API when it is there (and matches `hash`), else downloaded and kept. */
async function asset(cache: Cache | null, url: string, hash?: string): Promise<ArrayBuffer> {
  const kept = await cache?.match(url);
  if (kept) {
    const bytes = await kept.arrayBuffer();
    if (!hash || (await sha256(bytes)) === hash) return bytes;
  }
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: HTTP ${response.status}`);
  const bytes = await response.arrayBuffer();
  if (hash && (await sha256(bytes)) !== hash) throw new Error(`${url}: not the pinned model`);
  await cache?.put(url, new Response(bytes.slice(0), { headers: { "content-type": response.headers.get("content-type") ?? "application/octet-stream" } })).catch(() => {});
  return bytes;
}

async function build({ code: codeUrl, archive: archiveUrl, sha256: pin, cache: name, worklet }: Build): Promise<void> {
  const send = (message: Handover, transfer: Transferable[] = []) => worklet.postMessage(message, transfer);
  let exports: ModelExports | null = null;
  // The worklet asks for the model once it has compiled the code, then for each chunk.
  let chunks: Generator<{ offset: number; bytes: ArrayBuffer }> | null = null;
  let model: { size: number; denoiser: number } | null = null;
  let asked = false;
  const next = () => {
    if (!model) return;
    if (!chunks) {
      chunks = memoryChunks(exports!.memory);
      return send({ type: "model", ...model });
    }
    const chunk = chunks.next();
    if (chunk.done) return;
    // Before sending: the transfer leaves the bytes empty here.
    const last = chunk.value.offset + chunk.value.bytes.byteLength >= model.size;
    send({ type: "memory", ...chunk.value }, [chunk.value.bytes]);
    if (last) self.close();
  };
  worklet.onmessage = (event: MessageEvent<Next>) => {
    if (event.data.type !== "next") return;
    asked = true;
    next();
  };

  const cache = typeof caches === "undefined" ? null : await caches.open(name).catch(() => null);
  const archive = asset(cache, archiveUrl, pin);
  // Seen below; a failure of the code first fails the build alone.
  archive.catch(() => {});
  const code = await asset(cache, codeUrl);
  // Earlier builds' copies.
  if (cache) void cache.keys().then(requests => { for (const request of requests) if (request.url !== codeUrl && request.url !== archiveUrl) void cache.delete(request); }).catch(() => {});
  const copy = code.slice(0);
  send({ type: "code", code: copy }, [copy]);
  const module = await WebAssembly.compile(code);
  const instance = await WebAssembly.instantiate(module, {});
  exports = instance.exports as unknown as ModelExports;
  const denoiser = buildModel(exports, new Uint8Array(await archive));
  model = { size: exports.memory.buffer.byteLength, denoiser };
  if (asked) next();
}

self.onmessage = (event: MessageEvent<Build>) => {
  self.onmessage = null;
  build(event.data).catch((error: unknown) => {
    self.postMessage({ type: "failed", reason: error instanceof Error ? error.message : String(error) } satisfies BuildFailed);
    self.close();
  });
};
