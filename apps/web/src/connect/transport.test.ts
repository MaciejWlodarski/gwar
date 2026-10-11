import { createServer } from "node:http";
import { describe, expect, it, vi } from "vitest";
import { connectJson, connectUrl } from "./transport";

describe("Connect transport", () => {
  it("requires HTTPS without credentials, except dev loopback", () => {
    for (const base of ["http://connect.example", "https://u:p@connect.example", "file:///x", "https://connect.example?q=x"]) expect(() => connectUrl(base, "/v1/account", true)).toThrow();
    expect(() => connectUrl("http://127.0.0.1:8900", "/v1/account", false)).toThrow();
    expect(connectUrl("http://[::1]:8900", "/v1/account", true)).toBe("http://[::1]:8900/v1/account");
    expect(connectUrl("https://connect.example/connect/", "/v1/account", false)).toBe("https://connect.example/connect/v1/account");
  });
  it("rejects redirects and omits ambient credentials", async () => {
    const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValue(new Response("{}"));
    await connectJson("https://connect.example", "/v1/account", { credentials: "include", redirect: "follow" }, fetch);
    expect(fetch).toHaveBeenCalledWith("https://connect.example/v1/account", expect.objectContaining({ credentials: "omit", redirect: "error", cache: "no-store" }));
    fetch.mockResolvedValue(new Response(null, { status: 302 }));
    await expect(connectJson("https://connect.example", "/v1/account", {}, fetch)).rejects.toThrow("redirect");
  });
  it("times out requests and bounds response bodies", async () => {
    const pending = vi.fn<typeof fetch>().mockImplementation((_url, init) => new Promise((_r, reject) => init?.signal?.addEventListener("abort", () => reject(init.signal?.reason))));
    await expect(connectJson("https://connect.example", "/v1/account", {}, pending, 10)).rejects.toThrow("timed out");
    const large = vi.fn<typeof fetch>().mockResolvedValue(new Response("{}", { headers: { "Content-Length": "5000000" } }));
    await expect(connectJson("https://connect.example", "/v1/account", {}, large)).rejects.toThrow("too large");
  });
});

it("holds the Connect deadline through the JSON body", async () => {
  const server = createServer((_req, res) => { res.writeHead(200); res.flushHeaders(); res.write("{"); });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("no server address");
  try {
    await expect(connectJson(`http://127.0.0.1:${address.port}`, "/v1/account", {}, fetch, 50)).rejects.toThrow();
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});
