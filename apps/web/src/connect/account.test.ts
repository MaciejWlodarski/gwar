import { describe, expect, it } from "vitest";
import { decodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { changePassword, createAccount, normalizeHandle, recover, renewCertificate, revokeDevice, signIn, signOut } from "./account";
import { ConnectApi, ConnectApiError } from "./api";
import { deviceStatement, keyFromSeed, revokeStatement } from "./crypto";
import { deviceName } from "./device-name";

/** A tiny in-memory Connect service speaking the documented JSON API. It checks signatures like the real one. */
class FakeService {
  accounts = new Map<string, { account_key: string; kdf: unknown; auth_key: string; key_blob: string; recovery_auth: string; recovery_blob: string }>();
  devices: Array<{ handle: string; device_key: string; name: string; revoked_at: number | null }> = [];
  sessions = new Map<string, string>();
  n = 0;

  fetch = async (url: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const path = String(url).replace(/^.*\/v1/, "");
    const body = init?.body ? JSON.parse(init.body as string) : {};
    const auth = (init?.headers as Record<string, string> | undefined)?.Authorization?.replace("Bearer ", "");
    const handleOfSession = auth ? this.sessions.get(auth) : undefined;
    const ok = (data: unknown) => new Response(JSON.stringify(data), { status: 200 });
    const err = (status: number, error: string) => new Response(JSON.stringify({ error, message: error }), { status });
    const session = (handle: string) => {
      const t = `token-${++this.n}`;
      this.sessions.set(t, handle);
      return t;
    };
    const verify = async (accountKey: string, text: string, sig: string) => {
      const key = await crypto.subtle.importKey("raw", decodeBase64Url(accountKey) as BufferSource, { name: "Ed25519" }, false, ["verify"]);
      return crypto.subtle.verify({ name: "Ed25519" }, key, decodeBase64Url(sig) as BufferSource, new TextEncoder().encode(text));
    };
    if (path === "/register") {
      if (this.accounts.has(body.handle)) return err(409, "taken");
      const d = body.device;
      if (!(await verify(body.account_key, deviceStatement(body.account_key, d.device_key, d.issued_at, d.expires_at), d.signature))) return err(400, "bad_request");
      this.accounts.set(body.handle, body);
      this.devices.push({ handle: body.handle, device_key: d.device_key, name: d.name, revoked_at: null });
      return ok({ token: session(body.handle) });
    }
    if (path === "/prelogin") return ok({ kdf: this.accounts.get(body.handle)?.kdf ?? { salt: "AAAAAAAAAAAAAAAAAAAAAA", m: 19456, t: 2, p: 1 } });
    if (path === "/login") {
      const a = this.accounts.get(body.handle);
      if (!a || a.auth_key !== body.auth_key) return err(401, "unauthorized");
      return ok({ token: session(body.handle), account_key: a.account_key, key_blob: a.key_blob });
    }
    if (path === "/recover") {
      const a = this.accounts.get(body.handle);
      if (!a || a.recovery_auth !== body.recovery_auth) return err(401, "unauthorized");
      return ok({ token: session(body.handle), account_key: a.account_key, recovery_blob: a.recovery_blob });
    }
    if (path.startsWith("/accounts/")) {
      const a = this.accounts.get(decodeURIComponent(path.slice(10)));
      return a ? ok({ handle: path.slice(10), account_key: a.account_key }) : err(404, "not_found");
    }
    if (!handleOfSession) return err(401, "unauthorized");
    const a = this.accounts.get(handleOfSession)!;
    if (path === "/account/password") {
      Object.assign(a, { kdf: body.kdf, auth_key: body.auth_key, key_blob: body.key_blob });
      return ok({});
    }
    if (path === "/devices" && init?.method === "GET") return ok({ devices: this.devices.filter((d) => d.handle === handleOfSession) });
    if (path === "/devices") {
      if (!(await verify(a.account_key, deviceStatement(a.account_key, body.device_key, body.issued_at, body.expires_at), body.signature))) return err(400, "bad_request");
      const known = this.devices.find((d) => d.device_key === body.device_key);
      if (known?.revoked_at) return err(410, "revoked");
      if (known) known.name = body.name;
      else this.devices.push({ handle: handleOfSession, device_key: body.device_key, name: body.name, revoked_at: null });
      return ok({});
    }
    if (path === "/devices/revoke") {
      if (!(await verify(a.account_key, revokeStatement(a.account_key, body.device_key, body.revoked_at), body.signature))) return err(400, "bad_request");
      this.devices.find((d) => d.device_key === body.device_key)!.revoked_at = body.revoked_at;
      return ok({});
    }
    if (path === "/logout") {
      this.sessions.delete(auth!);
      return ok({});
    }
    return err(404, "not_found");
  };
}

function setup() {
  const service = new FakeService();
  let stored: ConnectRecord | undefined;
  const store: ConnectStore = { load: async () => stored, save: async (r) => void (stored = r), clear: async () => void (stored = undefined) };
  const deps = (name = "Chrome on macOS") => ({ api: new ConnectApi("http://c", service.fetch as typeof fetch), store, deviceName: name });
  return { service, store, deps, stored: () => stored };
}

describe("account flows", () => {
  it("normalizes handles like the service", () => {
    expect(normalizeHandle("@Maciej")).toBe("maciej");
    expect(normalizeHandle("a.b_c9")).toBe("a.b_c9");
    for (const bad of ["ab", "has space", ".dot", "dot.", "ąę", "x".repeat(33)]) expect(normalizeHandle(bad)).toBeNull();
  });

  it("creating an account that keeps the identity uses the local key as the account key; a second device gets the same one", async () => {
    const { service, deps } = setup();
    const local = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["sign", "verify"]);
    const localJwk = await crypto.subtle.exportKey("jwk", (local as CryptoKeyPair).privateKey);

    const created = await createAccount({ handle: "Alice", password: "correct horse", keepIdentity: localJwk }, deps("Chrome on macOS"));
    expect(created.record.accountKey).toBe(localJwk.x);
    expect(created.record.certificate.device_key).not.toBe(localJwk.x);
    expect(created.recoveryCode).toMatch(/^([A-Z2-7]{4}-){7}[A-Z2-7]{4}$/);
    expect(service.accounts.get("alice")?.account_key).toBe(localJwk.x);

    const phone = await signIn({ handle: "@ALICE", password: "correct horse" }, deps("Firefox on Linux"));
    expect(phone.accountKey).toBe(created.record.accountKey);
    expect(phone.certificate.device_key).not.toBe(created.record.certificate.device_key);
    expect(phone.certificate.expires_at - phone.certificate.issued_at).toBe(365 * 86_400_000);
    expect(service.devices.map((d) => d.name)).toEqual(["Chrome on macOS", "Firefox on Linux"]);
  }, 30_000);

  it("starting fresh makes a new account key", async () => {
    const { deps } = setup();
    const local = await keyFromSeed(new Uint8Array(32).fill(5));
    const created = await createAccount({ handle: "bob", password: "pw-long-enough" }, deps());
    expect(created.record.accountKey).not.toBe(local.publicKey);
  }, 30_000);

  it("refuses a taken handle and a wrong password", async () => {
    const { deps } = setup();
    await createAccount({ handle: "carol", password: "right password" }, deps());
    await expect(createAccount({ handle: "carol", password: "x-long-enough" }, deps())).rejects.toMatchObject({ kind: "taken" });
    await expect(signIn({ handle: "carol", password: "wrong" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    await expect(signIn({ handle: "no such!", password: "x" }, deps())).rejects.toBeInstanceOf(ConnectApiError);
  }, 30_000);

  it("revokes another device with the password, and refuses the wrong one", async () => {
    const { service, deps, stored } = setup();
    const first = (await createAccount({ handle: "dave", password: "right password" }, deps("A"))).record;
    const second = await signIn({ handle: "dave", password: "right password" }, deps("B"));
    await expect(revokeDevice(first, second.certificate.device_key, "wrong", deps())).rejects.toMatchObject({ kind: "unauthorized" });
    const next = await revokeDevice(first, second.certificate.device_key, "right password", deps());
    expect(service.devices.find((d) => d.name === "B")?.revoked_at).not.toBeNull();
    expect(service.devices.find((d) => d.name === "A")?.revoked_at).toBeNull();
    expect(stored()?.token).toBe(next.token);
  }, 30_000);

  it("changes the password and the old one stops working", async () => {
    const { deps } = setup();
    const rec = (await createAccount({ handle: "erin", password: "old password" }, deps())).record;
    await changePassword(rec, { password: "old password", newPassword: "new password" }, deps());
    await expect(signIn({ handle: "erin", password: "old password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    expect((await signIn({ handle: "erin", password: "new password" }, deps())).accountKey).toBe(rec.accountKey);
  }, 60_000);

  it("recovers with the code (any spelling), sets a new password and enrols the device", async () => {
    const { deps } = setup();
    const created = await createAccount({ handle: "frank", password: "forgotten" }, deps());
    const spelled = created.recoveryCode.toLowerCase().replace(/-/g, " ");
    await expect(recover({ handle: "frank", code: "AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA-AAAA", newPassword: "fresh password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    await expect(recover({ handle: "nobody", code: spelled, newPassword: "fresh password" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
    const rec = await recover({ handle: "frank", code: spelled, newPassword: "fresh password" }, deps("Phone"));
    expect(rec.accountKey).toBe(created.record.accountKey);
    expect((await signIn({ handle: "frank", password: "fresh password" }, deps())).accountKey).toBe(rec.accountKey);
    await expect(signIn({ handle: "frank", password: "forgotten" }, deps())).rejects.toMatchObject({ kind: "unauthorized" });
  }, 60_000);

  it("renews the certificate of the same device key", async () => {
    const { deps } = setup();
    const rec = (await createAccount({ handle: "gina", password: "right password" }, deps())).record;
    await new Promise((r) => setTimeout(r, 5));
    const next = await renewCertificate(rec, "right password", deps());
    expect(next.certificate.device_key).toBe(rec.certificate.device_key);
    expect(next.certificate.expires_at).toBeGreaterThan(rec.certificate.expires_at);
  }, 30_000);

  it("signing out forgets the record and ends the session", async () => {
    const { service, deps, stored } = setup();
    const rec = (await createAccount({ handle: "hank", password: "right password" }, deps())).record;
    await deps().store.save(rec);
    await signOut(rec, deps());
    expect(stored()).toBeUndefined();
    expect(service.sessions.has(rec.token)).toBe(false);
  }, 30_000);
});

describe("device names", () => {
  const chromeMac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";
  const edgeWin = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0";
  const firefoxLinux = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0";
  const safariIos = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1";
  it("names browsers and the desktop app with the OS", () => {
    expect(deviceName(chromeMac, false)).toBe("Chrome on macOS");
    expect(deviceName(edgeWin, false)).toBe("Edge on Windows");
    expect(deviceName(firefoxLinux, false)).toBe("Firefox on Linux");
    expect(deviceName(safariIos, false)).toBe("Safari on iOS");
    expect(deviceName(edgeWin, true)).toBe("Gwar desktop on Windows");
    expect(deviceName(chromeMac, true)).toBe("Gwar desktop on macOS");
  });
});
