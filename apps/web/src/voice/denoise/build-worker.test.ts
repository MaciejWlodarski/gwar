import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BuilderReply, BuilderRequest, Handover, Next } from "./protocol";
import { CHUNK } from "./snapshot";

const buildModel = vi.hoisted(() => vi.fn(() => 123));
vi.mock("./snapshot", async original => ({ ...await original<typeof import("./snapshot")>(), buildModel }));

class Port {
  onmessage: ((event: MessageEvent<Next>) => void) | null = null;
  sent: Handover[] = [];
  close = vi.fn();
  postMessage(message: Handover, transfer: Transferable[] = []) { this.sent.push(structuredClone(message, { transfer })); }
  next(chunks = 1) { this.onmessage?.({ data: { type: "next", chunks } } as MessageEvent<Next>); }
}
const scope = globalThis as unknown as { onmessage(event: { data: BuilderRequest }): void };
const attach = (id: number, port: Port) => scope.onmessage({ data: { type: "attach", id, worklet: port as unknown as MessagePort } });

describe("the model worker's pristine snapshot", () => {
  let fetch: ReturnType<typeof vi.fn>;
  let replies: BuilderReply[];
  let memory: ArrayBuffer;
  let start: () => void;
  let close: ReturnType<typeof vi.fn>;
  beforeEach(async () => {
    vi.resetModules(); buildModel.mockClear();
    memory = new ArrayBuffer(CHUNK * 9);
    new Uint8Array(memory).set([1, 2, 3]);
    new Uint8Array(memory)[memory.byteLength - 1] = 77;
    const archive = new Uint8Array([4, 5, 6]);
    const hash = [...new Uint8Array(await crypto.subtle.digest("SHA-256", archive))].map(b => b.toString(16).padStart(2, "0")).join("");
    fetch = vi.fn(async (url: string) => new Response(url === "model" ? archive : new Uint8Array([8, 9])));
    replies = []; close = vi.fn();
    vi.stubGlobal("fetch", fetch);
    vi.stubGlobal("caches", undefined);
    vi.stubGlobal("onmessage", null);
    vi.stubGlobal("postMessage", (message: BuilderReply) => replies.push(message));
    vi.stubGlobal("close", close);
    vi.stubGlobal("scheduler", { postTask: vi.fn(async (run: () => unknown, options: unknown) => { expect(options).toEqual({ priority: "background" }); return run(); }) });
    vi.spyOn(WebAssembly, "compile").mockResolvedValue({} as WebAssembly.Module);
    vi.spyOn(WebAssembly, "instantiate").mockResolvedValue({ exports: { memory: { buffer: memory } } } as unknown as WebAssembly.Instance);
    await import("./build-worker");
    start = () => scope.onmessage({ data: { type: "build", code: "wasm", archive: "model", sha256: hash, cache: "test" } });
  });
  afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); vi.resetModules(); });

  const ready = async () => { start(); await vi.waitFor(() => expect(replies).toEqual([{ type: "ready", bytes: CHUNK * 9 }])); };
  const copy = (id: number) => {
    const port = new Port(); attach(id, port);
    port.next(); port.next(8); port.next();
    return port;
  };

  it("builds without a worklet and reuses one unchanged snapshot for multiple joins", async () => {
    await ready();
    start();
    const first = copy(1);
    const firstBytes = first.sent.filter((m): m is Extract<Handover, { type: "memory" }> => m.type === "memory");
    expect(firstBytes).toHaveLength(9);
    new Uint8Array(firstBytes[0]!.bytes).fill(255);
    const second = copy(2);
    const bytes = second.sent.filter((m): m is Extract<Handover, { type: "memory" }> => m.type === "memory");
    expect(bytes).toHaveLength(9);
    expect([...new Uint8Array(bytes[0]!.bytes).subarray(0, 3)]).toEqual([1, 2, 3]);
    expect(new Uint8Array(bytes[8]!.bytes).at(-1)).toBe(77);
    expect(memory.byteLength).toBe(CHUNK * 9);
    expect(buildModel).toHaveBeenCalledOnce();
    expect(fetch).toHaveBeenCalledTimes(2);
    expect(close).not.toHaveBeenCalled();
    expect(first.close).toHaveBeenCalledOnce(); expect(second.close).toHaveBeenCalledOnce();
    for (const message of [...firstBytes, ...bytes]) expect(message.bytes.byteLength).toBeLessThanOrEqual(CHUNK);
  });

  it("starts code compilation before the build completes and hands over only on credits", async () => {
    const port = new Port(); attach(1, port); port.next();
    await ready();
    expect(port.sent.map(m => m.type)).toEqual(["code", "model"]);
    port.next(8);
    expect(port.sent.filter(m => m.type === "memory")).toHaveLength(8);
    for (const invalid of [9, -1, NaN, Infinity, 1.5]) port.next(invalid);
    expect(port.sent.filter(m => m.type === "memory")).toHaveLength(8);
    port.next();
    expect(port.sent.filter(m => m.type === "memory")).toHaveLength(9);
  });

  it("detaches a cancelled copy without cancelling another copy or destroying the cache", async () => {
    await ready();
    const a = new Port(), b = new Port(); attach(1, a); attach(2, b);
    a.next(); b.next();
    scope.onmessage({ data: { type: "detach", id: 1 } });
    a.next(8); b.next(8); b.next();
    expect(a.sent.map(m => m.type)).toEqual(["code", "model"]);
    expect(b.sent.filter(m => m.type === "memory")).toHaveLength(9);
    expect(a.close).toHaveBeenCalledOnce(); expect(b.close).toHaveBeenCalledOnce();
    expect(buildModel).toHaveBeenCalledOnce(); expect(close).not.toHaveBeenCalled();
  });
});
