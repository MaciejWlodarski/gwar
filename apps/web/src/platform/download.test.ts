import { createServer } from "node:http";
import { describe, expect, it, vi } from "vitest";
import { fetchFileBytes, fileCap, MAX_FILE_BYTES } from "./download";

const options = { origin: "https://voice.example", size: 4, uploadLimit: 8 };
const mock = (response: Response) => vi.fn<typeof fetch>().mockResolvedValue(response);

describe("bounded file fetching", () => {
  it("uses only the server route and rejects redirects", async () => {
    const fetch = mock(new Response(new Uint8Array([1, 2, 3, 4])));
    expect(await fetchFileBytes("/files/a/b", options, fetch)).toEqual(new Uint8Array([1, 2, 3, 4]));
    expect(fetch).toHaveBeenCalledWith("https://voice.example/files/a/b", expect.objectContaining({ redirect: "error", credentials: "omit" }));
    await expect(fetchFileBytes("https://evil.example/a", options, fetch)).rejects.toThrow("Invalid file path");
    await expect(fetchFileBytes("/files/a/b", options, mock(new Response(null, { status: 302 })))).rejects.toThrow("302");
  });

  it("sends the download token only when given one", async () => {
    const fetch = mock(new Response(new Uint8Array(4)));
    await fetchFileBytes("/files/a/b", { ...options, access: "tok" }, fetch);
    expect(fetch.mock.calls[0]?.[1]?.headers).toEqual({ Authorization: "Bearer tok" });
    const open = mock(new Response(new Uint8Array(4)));
    await fetchFileBytes("/files/a/b", options, open);
    expect(open.mock.calls[0]?.[1]?.headers).toBeUndefined();
  });

  it("caps both declared length and streamed bytes and cancels overruns", async () => {
    await expect(fetchFileBytes("/files/a/b", options, mock(new Response("x", { headers: { "Content-Length": "5" } })))).rejects.toThrow("size limit");
    const cancel = vi.fn();
    const body = new ReadableStream({ start(c) { c.enqueue(new Uint8Array(5)); }, cancel });
    await expect(fetchFileBytes("/files/a/b", options, mock(new Response(body)))).rejects.toThrow("size limit");
    expect(cancel).toHaveBeenCalledOnce();
    expect(() => fileCap(MAX_FILE_BYTES + 1, Number.MAX_SAFE_INTEGER)).toThrow();
    expect(() => fileCap(9, 8)).toThrow();
    expect(() => fileCap(NaN, 8)).toThrow();
  });

  it("times out and supports caller cancellation", async () => {
    const pending = vi.fn<typeof fetch>().mockImplementation((_url, init) => new Promise((_resolve, reject) => {
      const signal = init?.signal;
      if (signal?.aborted) reject(signal.reason);
      signal?.addEventListener("abort", () => reject(signal.reason), { once: true });
    }));
    await expect(fetchFileBytes("/files/a/b", options, pending, 10)).rejects.toThrow("timed out");
    const controller = new AbortController();
    const request = fetchFileBytes("/files/a/b", { ...options, signal: controller.signal }, pending);
    controller.abort(new Error("unmounted"));
    await expect(request).rejects.toThrow("unmounted");
  });
});

it("enforces the deadline through a stalled response body and rejects real redirects", async () => {
  let followed = false;
  const server = createServer((req, res) => {
    if (req.url === "/files/a/redirect") { res.writeHead(302, { Location: "/side-effect" }); res.end(); }
    else if (req.url === "/side-effect") { followed = true; res.end("no"); }
    else { res.writeHead(200, { "Content-Length": "4" }); res.flushHeaders(); res.write("1"); }
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("no server address");
  const local = { ...options, origin: `http://127.0.0.1:${address.port}` };
  try {
    await expect(fetchFileBytes("/files/a/redirect", local)).rejects.toThrow();
    expect(followed).toBe(false);
    await expect(fetchFileBytes("/files/a/slow", local, fetch, 50)).rejects.toThrow();
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});
