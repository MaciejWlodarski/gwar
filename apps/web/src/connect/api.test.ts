import { describe, expect, it, vi } from "vitest";
import { ConnectApi, ConnectApiError } from "./api";

function reply(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function make(handler: (url: string, init: RequestInit) => Response | Promise<Response>) {
  const fetchFn = vi.fn(async (url: string | URL | Request, init?: RequestInit) => handler(String(url), init ?? {}));
  return { api: new ConnectApi("https://c.example/connect", fetchFn as unknown as typeof fetch), fetchFn };
}

describe("ConnectApi", () => {
  it("posts JSON under /v1 and unwraps the kdf", async () => {
    const kdf = { salt: "AAAA", m: 65536, t: 3, p: 1 };
    const { api, fetchFn } = make(() => reply(200, { kdf }));
    expect(await api.prelogin("alice")).toEqual(kdf);
    const [url, init] = fetchFn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("https://c.example/connect/v1/prelogin");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ handle: "alice" });
    expect((init.headers as Record<string, string>)["Content-Type"]).toBe("application/json");
  });

  it("sends the bearer token on authenticated calls only", async () => {
    const { api, fetchFn } = make(() => reply(200, { devices: [] }));
    await api.devices("tok");
    await api.revocations(7);
    const calls = fetchFn.mock.calls as unknown as Array<[string, RequestInit]>;
    expect((calls[0]![1].headers as Record<string, string>).Authorization).toBe("Bearer tok");
    expect(calls[1]![0]).toBe("https://c.example/connect/v1/revocations?since=7");
    expect((calls[1]![1].headers as Record<string, string>).Authorization).toBeUndefined();
  });

  it("uses the documented paths and methods", async () => {
    const { api, fetchFn } = make(() => reply(200, {}));
    await api.changePassword("t", { salt: "s", m: 1, t: 1, p: 1 }, "a", "b");
    await api.addDevice("t", { device_key: "d", name: "n", issued_at: 1, expires_at: 2, signature: "s" });
    await api.renewDevices("t", [{ device_key: "d", issued_at: 1, expires_at: 2, signature: "s" }]);
    await api.revokeDevice("t", { device_key: "d", revoked_at: 1, signature: "s" });
    await api.logout("t");
    await api.getVault("t");
    await api.putVault("t", "blob", 3);
    await api.publicAccount("@a b");
    const seen = (fetchFn.mock.calls as unknown as Array<[string, RequestInit]>).map(([u, i]) => `${i.method} ${u.replace("https://c.example/connect", "")}`);
    expect(seen).toEqual([
      "PUT /v1/account/password",
      "POST /v1/devices",
      "POST /v1/devices/renew",
      "POST /v1/devices/revoke",
      "POST /v1/logout",
      "GET /v1/vault",
      "PUT /v1/vault",
      "GET /v1/accounts/%40a%20b",
    ]);
  });

  it("renews with a list of certificates and reads the count", async () => {
    const { api, fetchFn } = make(() => reply(200, { renewed: 2 }));
    const certificates = [
      { device_key: "a", issued_at: 1, expires_at: 2, signature: "s" },
      { device_key: "b", issued_at: 1, expires_at: 2, signature: "t" },
    ];
    expect(await api.renewDevices("tok", certificates)).toBe(2);
    const [, init] = fetchFn.mock.calls[0] as unknown as [string, RequestInit];
    expect(JSON.parse(init.body as string)).toEqual({ certificates });
    expect((init.headers as Record<string, string>).Authorization).toBe("Bearer tok");
  });

  it("lists devices with their newest certificate", async () => {
    const device = { device_key: "a", name: "n", created_at: 1, last_seen: 2, revoked_at: null, certificate: { issued_at: 1, expires_at: 2, signature: "s" } };
    const { api } = make(() => reply(200, { devices: [device, { ...device, device_key: "b", certificate: null }] }));
    const list = await api.devices("tok");
    expect(list[0]!.certificate).toEqual({ issued_at: 1, expires_at: 2, signature: "s" });
    expect(list[1]!.certificate).toBeNull();
  });

  it("reads and writes the vault with the version it was based on", async () => {
    const { api, fetchFn } = make((_url, init) =>
      init.method === "GET" ? reply(200, { vault: null, version: 0, updated_at: 0 }) : reply(200, { version: 4 }),
    );
    expect(await api.getVault("tok")).toEqual({ vault: null, version: 0, updated_at: 0 });
    expect(await api.putVault("tok", "blob", 3)).toBe(4);
    const [, put] = fetchFn.mock.calls[1] as unknown as [string, RequestInit];
    expect(JSON.parse(put.body as string)).toEqual({ vault: "blob", version: 3 });
    expect((put.headers as Record<string, string>).Authorization).toBe("Bearer tok");
  });

  it("maps service errors to kinds", async () => {
    const cases: Array<[number, string, string]> = [
      [401, "unauthorized", "unauthorized"],
      [429, "rate_limited", "rate_limited"],
      [409, "taken", "taken"],
      [409, "conflict", "conflict"],
      [410, "revoked", "revoked"],
      [404, "not_found", "not_found"],
      [400, "bad_request", "bad_request"],
      [500, "internal", "server"],
    ];
    for (const [status, code, kind] of cases) {
      const { api } = make(() => reply(status, { error: code, message: "msg" }));
      await expect(api.login("a", "b")).rejects.toMatchObject({ kind, status, message: "msg" });
    }
  });

  it("treats a bare 429 as rate limited and a network failure as network", async () => {
    await expect(make(() => new Response("slow down", { status: 429 })).api.account("t")).rejects.toMatchObject({ kind: "rate_limited" });
    const { api } = make(() => {
      throw new TypeError("Failed to fetch");
    });
    const err = await api.account("t").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ConnectApiError);
    expect(err).toMatchObject({ kind: "network" });
  });
});
