import type { Attachment } from "../proto/Attachment";
import { isDesktop, tauri } from ".";
import { absoluteUrl, filePath } from "../net/address";

export const MAX_FILE_BYTES = 30 * 1024 * 1024;
export const FILE_TIMEOUT_MS = 30_000;

let nativeReady: Promise<unknown> = Promise.resolve();

/** Ordered native connection state updates; file commands wait for them. */
export function setConnectedServer(origin: string | null, uploadLimit = 0): Promise<unknown> {
  if (!isDesktop()) return Promise.resolve();
  nativeReady = nativeReady.catch(() => {}).then(async () => (await tauri()).invoke("files_set_server", { origin, uploadLimit }));
  return nativeReady;
}

export interface FileOptions {
  origin: string | null;
  size: number;
  uploadLimit: number;
  signal?: AbortSignal;
}

export function fileCap(size: number, uploadLimit: number): number {
  if (!Number.isSafeInteger(size) || size <= 0 || !Number.isSafeInteger(uploadLimit) || uploadLimit <= 0 || size > Math.min(uploadLimit, MAX_FILE_BYTES)) {
    throw new Error("File exceeds size limit");
  }
  return size;
}

/** Streams within the declared size, server limit and local ceiling. Used before display or saving. */
export async function fetchFileBytes(path: string, opts: FileOptions, fetchFn: typeof fetch = fetch, timeoutMs = FILE_TIMEOUT_MS): Promise<Uint8Array<ArrayBuffer>> {
  const cap = fileCap(opts.size, opts.uploadLimit);
  const url = absoluteUrl(opts.origin, path);
  const controller = new AbortController();
  const abort = () => controller.abort(opts.signal?.reason);
  opts.signal?.addEventListener("abort", abort, { once: true });
  if (opts.signal?.aborted) abort();
  const timer = setTimeout(() => controller.abort(new Error("File request timed out")), Math.min(timeoutMs, FILE_TIMEOUT_MS));
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    if (isDesktop()) {
      await nativeReady;
      controller.signal.throwIfAborted();
      const api = await tauri();
      controller.signal.throwIfAborted();
      const requestId = crypto.randomUUID();
      const cancel = () => { void api.invoke("files_cancel", { requestId }); };
      controller.signal.addEventListener("abort", cancel, { once: true });
      try {
        const raw = await api.invoke<ArrayBuffer>("fetch_file", { path, size: cap, uploadLimit: opts.uploadLimit, requestId });
        controller.signal.throwIfAborted();
        const bytes = new Uint8Array(raw);
        if (bytes.length > cap) throw new Error("File exceeds size limit");
        return bytes;
      } finally { controller.signal.removeEventListener("abort", cancel); }
    }
    const response = await fetchFn(url, { redirect: "error", credentials: "omit", signal: controller.signal });
    if (!response.ok || response.redirected) throw new Error(`File HTTP ${response.status}`);
    const length = response.headers.get("Content-Length");
    if (length !== null && (!/^\d+$/.test(length) || Number(length) > cap)) throw new Error("File exceeds size limit");
    if (!response.body) throw new Error("Missing file body");
    reader = response.body.getReader();
    const chunks: Uint8Array[] = [];
    let size = 0;
    while (true) {
      const next = await reader.read();
      controller.signal.throwIfAborted();
      if (next.done) break;
      size += next.value.length;
      if (size > cap) throw new Error("File exceeds size limit");
      chunks.push(next.value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.length; }
    return bytes;
  } catch (e) {
    controller.abort();
    await reader?.cancel().catch(() => {});
    throw e;
  } finally {
    clearTimeout(timer);
    opts.signal?.removeEventListener("abort", abort);
    reader?.releaseLock();
  }
}

export async function saveFile(path: string, name: string, opts: FileOptions): Promise<boolean> {
  const cap = fileCap(opts.size, opts.uploadLimit);
  absoluteUrl(opts.origin, path);
  if (isDesktop()) {
    await nativeReady;
    const api = await tauri();
    const requestId = crypto.randomUUID();
    const cancel = () => { void api.invoke("files_cancel", { requestId }); };
    opts.signal?.throwIfAborted();
    opts.signal?.addEventListener("abort", cancel, { once: true });
    try {
      return await api.invoke<boolean>("save_file_as", { path, fileName: name, size: cap, requestId });
    } finally { opts.signal?.removeEventListener("abort", cancel); }
  }
  const bytes = await fetchFileBytes(path, opts);
  const url = URL.createObjectURL(new Blob([bytes], { type: "application/octet-stream" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name.split(/[\\/]/).pop() || "file";
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
  return true;
}

/** Uploads exactly `bytes` to the server at `origin`, which reserved the upload. Native code refuses if it is no
 *  longer connected to that server; `stillHere` throws if the session that started the upload has ended. */
export async function uploadFromApp(path: string, bytes: Uint8Array, uploadLimit: number, origin: string, stillHere: () => void = () => {}): Promise<Attachment> {
  filePath(path, "PUT");
  fileCap(bytes.byteLength, uploadLimit);
  await nativeReady;
  const { invoke } = await import("@tauri-apps/api/core");
  stillHere();
  return invoke<Attachment>("upload_put", bytes, { headers: { "x-upload-path": path, "x-upload-origin": origin } });
}

/** Save the exact inspected bytes; native code never refetches a potentially substituted URL. */
export async function saveBlob(blob: Blob, name: string): Promise<boolean> {
  if (blob.size > MAX_FILE_BYTES) throw new Error("File exceeds size limit");
  if (isDesktop()) {
    const { invoke } = await import("@tauri-apps/api/core");
    return invoke<boolean>("save_blob_as", new Uint8Array(await blob.arrayBuffer()), { headers: { "x-file-name": encodeURIComponent(name) } });
  }
  const url = URL.createObjectURL(blob), a = document.createElement("a");
  a.href = url; a.download = name; document.body.appendChild(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
  return true;
}
