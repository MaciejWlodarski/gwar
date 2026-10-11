/** Connect transport policy shared by API calls and service-key fetching. */
export const CONNECT_TIMEOUT_MS = 10_000;
const MAX_JSON_BYTES = 4 * 1024 * 1024;

export function connectUrl(base: string, path: string, dev = import.meta.env.DEV): string {
  const url = new URL(base);
  const local = dev && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (url.username || url.password || url.search || url.hash || (url.protocol !== "https:" && !(url.protocol === "http:" && local))) {
    throw new Error("Connect requires HTTPS without URL credentials");
  }
  if (!path.startsWith("/v1/") || /[\\\s#]/.test(path)) throw new Error("Invalid Connect path");
  const request = new URL(`${url.href.replace(/\/+$/, "")}${path}`);
  if (request.origin !== url.origin) throw new Error("Invalid Connect origin");
  return request.href;
}

export async function connectJson(base: string, path: string, init: RequestInit = {}, fetchFn: typeof fetch = fetch, timeoutMs = CONNECT_TIMEOUT_MS): Promise<{ response: Response; data: unknown }> {
  const url = connectUrl(base, path);
  const controller = new AbortController();
  const abort = () => controller.abort(init.signal?.reason);
  init.signal?.addEventListener("abort", abort, { once: true });
  if (init.signal?.aborted) abort();
  const timer = setTimeout(() => controller.abort(new Error("Connect request timed out")), Math.min(timeoutMs, CONNECT_TIMEOUT_MS));
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
  try {
    const response = await fetchFn(url, { ...init, redirect: "error", credentials: "omit", cache: "no-store", signal: controller.signal });
    if (response.redirected || (response.status >= 300 && response.status < 400)) throw new Error("Connect redirect rejected");
    if (Number(response.headers.get("Content-Length")) > MAX_JSON_BYTES) throw new Error("Connect response too large");
    let content = "";
    if (response.body) {
      reader = response.body.getReader();
      const decoder = new TextDecoder();
      let size = 0;
      while (true) {
        const next = await reader.read();
        controller.signal.throwIfAborted();
        if (next.done) break;
        size += next.value.length;
        if (size > MAX_JSON_BYTES) throw new Error("Connect response too large");
        content += decoder.decode(next.value, { stream: true });
      }
      content += decoder.decode();
    }
    let data: unknown = null;
    try { data = JSON.parse(content); } catch { /* Empty and non-JSON error bodies. */ }
    return { response, data };
  } catch (e) {
    controller.abort();
    await reader?.cancel().catch(() => {});
    throw e;
  } finally {
    clearTimeout(timer);
    init.signal?.removeEventListener("abort", abort);
    reader?.releaseLock();
  }
}
