import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { encodeBase64Url } from "../net/base64url";

const key = encodeBase64Url(new Uint8Array(32).fill(1));
const next = encodeBase64Url(new Uint8Array(32).fill(2));
let storage: Map<string, string>;
beforeEach(() => {
  vi.resetModules();
  vi.stubEnv("VITE_CONNECT_URL", "https://connect.example");
  storage = new Map();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => storage.get(k) ?? null,
    setItem: (k: string, v: string) => storage.set(k, v),
  });
});
afterEach(() => { vi.unstubAllGlobals(); vi.unstubAllEnvs(); });

describe("service key trust anchor", () => {
  it("fetches only configured Connect over HTTPS, once per session, and caches by URL", async () => {
    const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify({ key })));
    vi.stubGlobal("fetch", fetcher);
    const { loadServiceKey } = await import("./service-key");
    expect(await Promise.all([loadServiceKey(), loadServiceKey()])).toEqual([key, key]);
    expect(fetcher).toHaveBeenCalledTimes(1);
    expect(fetcher).toHaveBeenCalledWith("https://connect.example/v1/service-key", expect.objectContaining({ redirect: "error", credentials: "omit", cache: "no-store" }));
    expect(storage.get("gwar.connect.service-key:https://connect.example")).toBe(key);
    vi.resetModules();
    fetcher.mockResolvedValue(new Response(JSON.stringify({ key: next })));
    expect(await (await import("./service-key")).loadServiceKey()).toBe(next);
    expect(fetcher).toHaveBeenCalledTimes(2);
  });
  it("uses a previously fetched key during an outage", async () => {
    storage.set("gwar.connect.service-key:https://connect.example", key);
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    expect(await (await import("./service-key")).loadServiceKey()).toBe(key);
  });
  it("hides handles when no key is available and never borrows another URL's cache", async () => {
    storage.set("gwar.connect.service-key:https://other.example", key);
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("offline")));
    expect(await (await import("./service-key")).loadServiceKey()).toBeNull();
  });
  it.each([{}, { key: "malformed" }, { key: encodeBase64Url(new Uint8Array(31)) }])("rejects malformed key reply %j", async (body) => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(new Response(JSON.stringify(body))));
    expect(await (await import("./service-key")).loadServiceKey()).toBeNull();
  });
  it("rejects remote HTTP", async () => {
    vi.stubEnv("VITE_CONNECT_URL", "http://connect.example");
    const fetcher = vi.fn();
    vi.stubGlobal("fetch", fetcher);
    expect(await (await import("./service-key")).loadServiceKey()).toBeNull();
    expect(fetcher).not.toHaveBeenCalled();
  });
  it("allows local HTTP for the development smoke test", async () => {
    vi.stubEnv("VITE_CONNECT_URL", "http://127.0.0.1:8900");
    const fetcher = vi.fn().mockResolvedValue(new Response(JSON.stringify({ key })));
    vi.stubGlobal("fetch", fetcher);
    expect(await (await import("./service-key")).loadServiceKey()).toBe(key);
    expect(fetcher).toHaveBeenCalledWith("http://127.0.0.1:8900/v1/service-key", expect.anything());
  });
});
