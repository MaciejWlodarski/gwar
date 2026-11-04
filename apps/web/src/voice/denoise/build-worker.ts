/**
 * Builds the noise suppression's model off the page and off the audio thread: downloads (or takes
 * from the Cache API) the module and the model's archive, checks the archive against its pin,
 * builds the model, and hands the module's memory to the worklet a chunk at a time (snapshot.ts).
 */
import { buildModel, memoryChunks, type ModelExports } from "./snapshot";
import type { Build, BuilderReply, BuilderRequest, Handover, Next } from "./protocol";

/** The worker's global scope, without the webworker lib (it would retype the app's globals). */
interface WorkerScope {
  onmessage: ((event: MessageEvent<BuilderRequest>) => void) | null;
  postMessage(message: BuilderReply): void;
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
  // Kept for the next load while this one builds: the disk write is not on the way to a call.
  void cache?.put(url, new Response(bytes.slice(0), { headers: { "content-type": response.headers.get("content-type") ?? "application/octet-stream" } })).catch(() => {});
  return bytes;
}

interface Model { exports: ModelExports; denoiser: number; size: number }
interface Copy { port: MessagePort; chunks: ReturnType<typeof memoryChunks> | null; credits: number }
const copies = new Map<number, Copy>();
let model: Model | null = null;
let code: ArrayBuffer | null = null;
let started = false;

function detach(id: number): void {
  const copy = copies.get(id);
  if (!copy) return;
  copies.delete(id);
  copy.port.onmessage = null;
  copy.port.close();
  copy.chunks?.return(undefined);
}

function pump(id: number, copy: Copy): void {
  if (!model) return;
  const send = (message: Handover, transfer: Transferable[] = []) => copy.port.postMessage(message, transfer);
  while (copy.credits > 0) {
    copy.credits--;
    if (!copy.chunks) {
      copy.chunks = memoryChunks(model.exports.memory);
      send({ type: "model", size: model.size, denoiser: model.denoiser });
      continue;
    }
    const chunk = copy.chunks.next();
    if (chunk.done) return detach(id);
    const last = chunk.value.offset + chunk.value.bytes.byteLength >= model.size;
    send({ type: "memory", ...chunk.value }, [chunk.value.bytes]);
    if (last) return detach(id);
  }
}

function sendCode(copy: Copy): void {
  if (!code) return;
  const bytes = code.slice(0);
  copy.port.postMessage({ type: "code", code: bytes } satisfies Handover, [bytes]);
}

/** Lower-priority work where supported; no expensive operation runs on the page. */
function background<T>(run: () => T): Promise<T> {
  const scheduler = (globalThis as unknown as { scheduler?: { postTask<T>(run: () => T, options: { priority: "background" }): Promise<T> } }).scheduler;
  if (scheduler) return scheduler.postTask(run, { priority: "background" });
  return new Promise((resolve, reject) => setTimeout(() => { try { resolve(run()); } catch (error) { reject(error); } }, 0));
}

async function build({ code: codeUrl, archive: archiveUrl, sha256: pin, cache: name }: Build): Promise<void> {
  const cache = typeof caches === "undefined" ? null : await caches.open(name).catch(() => null);
  const archive = asset(cache, archiveUrl, pin);
  void archive.catch(() => {});
  code = await asset(cache, codeUrl);
  if (cache) void cache.keys().then(requests => { for (const request of requests) if (request.url !== codeUrl && request.url !== archiveUrl) void cache.delete(request); }).catch(() => {});
  for (const copy of copies.values()) sendCode(copy);
  const module = await WebAssembly.compile(code);
  const instance = await WebAssembly.instantiate(module, {});
  const exports = instance.exports as unknown as ModelExports;
  const bytes = new Uint8Array(await archive);
  const denoiser = await background(() => buildModel(exports, bytes));
  model = { exports, size: exports.memory.buffer.byteLength, denoiser };
  self.postMessage({ type: "ready", bytes: model.size });
  for (const [id, copy] of copies) pump(id, copy);
}

self.onmessage = (event: MessageEvent<BuilderRequest>) => {
  const request = event.data;
  if (request.type === "detach") return detach(request.id);
  if (request.type === "attach") {
    detach(request.id);
    const copy: Copy = { port: request.worklet, chunks: null, credits: 0 };
    copies.set(request.id, copy);
    copy.port.onmessage = (event: MessageEvent<Next>) => {
      if (event.data.type !== "next" || !copies.has(request.id)) return;
      const credits = event.data.chunks ?? 1;
      if (!Number.isInteger(credits) || credits < 1 || credits > 8) return;
      copy.credits = Math.min(8, copy.credits + credits);
      pump(request.id, copy);
    };
    sendCode(copy);
  } else if (!started) {
    started = true;
    build(request).catch((error: unknown) => {
      for (const id of copies.keys()) detach(id);
      self.postMessage({ type: "failed", reason: error instanceof Error ? error.message : String(error) });
      self.close();
    });
  }
};
