import { decodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { ConnectApi } from "./api";
import { deviceStatement, openVault, revokeStatement, sealVault } from "./crypto";

const DAY_MS = 86_400_000;

interface FakeCertificate {
  issued_at: number;
  expires_at: number;
  signature: string;
}

interface FakeDevice {
  handle: string;
  device_key: string;
  name: string;
  revoked_at: number | null;
  certificate: FakeCertificate | null;
}

/** A tiny in-memory Connect service speaking the documented JSON API. It checks signatures like the real one. */
export class FakeService {
  accounts = new Map<string, { account_key: string; kdf: unknown; auth_key: string; key_blob: string; recovery_auth: string; recovery_blob: string }>();
  devices: FakeDevice[] = [];
  /** Every `POST /v1/devices/renew` body received, to see what a client sent. */
  renewals: Array<{ certificates: Array<{ device_key: string; issued_at: number; expires_at: number; signature: string }> }> = [];
  /** Every request path (with method), in order. */
  requests: string[] = [];
  sessions = new Map<string, string>();
  /** The sealed vault per handle, and its version (0 before the first write). */
  vaults = new Map<string, { vault: string; version: number }>();
  n = 0;

  fetch = async (url: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const path = String(url).replace(/^.*\/v1/, "");
    this.requests.push(`${init?.method ?? "GET"} ${path}`);
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
    // Like the service: a certificate replaces the stored one only if it lasts longer, and never for a revoked device.
    const setCertificate = (device: FakeDevice, c: FakeCertificate) => {
      if (device.revoked_at !== null || (device.certificate && device.certificate.expires_at >= c.expires_at)) return false;
      device.certificate = { issued_at: c.issued_at, expires_at: c.expires_at, signature: c.signature };
      return true;
    };
    // The service's check_certificate: lifetime and signature.
    const certificateProblem = async (accountKey: string, c: { device_key: string; issued_at: number; expires_at: number; signature: string }) => {
      if (c.expires_at <= Date.now() || c.expires_at - c.issued_at > 400 * DAY_MS) return true;
      return !(await verify(accountKey, deviceStatement(accountKey, c.device_key, c.issued_at, c.expires_at), c.signature));
    };
    if (path === "/register") {
      if (this.accounts.has(body.handle)) return err(409, "taken");
      const d = body.device;
      if (!(await verify(body.account_key, deviceStatement(body.account_key, d.device_key, d.issued_at, d.expires_at), d.signature))) return err(400, "bad_request");
      this.accounts.set(body.handle, body);
      this.devices.push({ handle: body.handle, device_key: d.device_key, name: d.name, revoked_at: null, certificate: { issued_at: d.issued_at, expires_at: d.expires_at, signature: d.signature } });
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
    if (path === "/devices" && init?.method === "GET") {
      return ok({ devices: this.devices.filter((d) => d.handle === handleOfSession).map((d) => ({ ...d, created_at: 1, last_seen: 1 })) });
    }
    if (path === "/devices") {
      if (!(await verify(a.account_key, deviceStatement(a.account_key, body.device_key, body.issued_at, body.expires_at), body.signature))) return err(400, "bad_request");
      const known = this.devices.find((d) => d.device_key === body.device_key);
      if (known?.revoked_at) return err(410, "revoked");
      if (known) {
        known.name = body.name;
        setCertificate(known, body);
      } else {
        this.devices.push({ handle: handleOfSession, device_key: body.device_key, name: body.name, revoked_at: null, certificate: { issued_at: body.issued_at, expires_at: body.expires_at, signature: body.signature } });
      }
      return ok({});
    }
    if (path === "/devices/renew") {
      const certificates: Array<{ device_key: string; issued_at: number; expires_at: number; signature: string }> = body.certificates ?? [];
      this.renewals.push({ certificates });
      if (certificates.length > 100) return err(400, "bad_request");
      let renewed = 0;
      for (const c of certificates) {
        if (await certificateProblem(a.account_key, c)) return err(400, "bad_request");
        const device = this.devices.find((d) => d.device_key === c.device_key);
        // Revoked, unknown or someone else's: skipped, not an error.
        if (device && device.handle === handleOfSession && setCertificate(device, c)) renewed++;
      }
      return ok({ renewed });
    }
    if (path === "/devices/revoke") {
      if (!(await verify(a.account_key, revokeStatement(a.account_key, body.device_key, body.revoked_at), body.signature))) return err(400, "bad_request");
      this.devices.find((d) => d.device_key === body.device_key)!.revoked_at = body.revoked_at;
      return ok({});
    }
    if (path === "/vault" && init?.method === "GET") {
      const v = this.vaults.get(handleOfSession);
      return ok({ vault: v?.vault ?? null, version: v?.version ?? 0, updated_at: 1 });
    }
    if (path === "/vault") {
      const have = this.vaults.get(handleOfSession)?.version ?? 0;
      if (body.version !== have) return err(409, "conflict");
      this.vaults.set(handleOfSession, { vault: body.vault, version: have + 1 });
      return ok({ version: have + 1 });
    }
    if (path === "/logout") {
      this.sessions.delete(auth!);
      return ok({});
    }
    return err(404, "not_found");
  };
}

export function setup() {
  const service = new FakeService();
  let stored: ConnectRecord | undefined;
  const store: ConnectStore = { load: async () => stored, save: async (r) => void (stored = r), clear: async () => void (stored = undefined) };
  const deps = (name = "Chrome on macOS") => ({ api: new ConnectApi("http://c", service.fetch as typeof fetch), store, deviceName: name });
  return { service, store, deps, stored: () => stored };
}


/** What the service holds for a record's account, decrypted with its vault key (null before the first write). */
export async function serverVault(service: FakeService, record: ConnectRecord): Promise<{ contents: unknown; version: number } | null> {
  const stored = service.vaults.get(record.handle);
  if (!stored) return null;
  const json = await openVault(decodeBase64Url(record.vaultKey!), stored.vault, record.accountKey);
  return { contents: JSON.parse(json), version: stored.version };
}

/** Writes the vault the way another device would, bypassing the client under test. */
export async function writeServerVault(service: FakeService, record: ConnectRecord, contents: unknown): Promise<void> {
  const have = service.vaults.get(record.handle)?.version ?? 0;
  const vault = await sealVault(decodeBase64Url(record.vaultKey!), JSON.stringify(contents), record.accountKey);
  service.vaults.set(record.handle, { vault, version: have + 1 });
}
