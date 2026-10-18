import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { ConnectRecord, ConnectStore } from "../net/identity";
import { ConnectApi } from "./api";
import { certificateHash, credentialsStatement, keyFromSeed, newKdf, openVault, revokeStatement, sealVault, sessionStatement, verifyDeviceCertificate, verifyText, type ConnectChallenge, type SignedDevice } from "./crypto";

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

/** In-memory API fixture; the real API regressions also exercise SQLite atomicity. */
export class FakeService {
  accounts = new Map<string, { account_key: string; kdf: unknown; auth_key: string; key_blob: string; recovery_auth: string; recovery_blob: string; credential_version: number }>();
  devices: FakeDevice[] = [];
  requests: string[] = [];
  sessions = new Map<string, string>();
  enrollmentTokens = new Set<string>();
  sessionDevices = new Map<string, string>();
  expiries = new Map<string, number>();
  challenges = new Map<string, { token: string; purpose: string; cert?: SignedDevice; reply: ConnectChallenge }>();
  vaults = new Map<string, { vault: string; version: number }>();
  /** The key archive by handle; shapes only, like Connect's checks minus signatures. */
  archive = new Map<string, {
    devices: Map<string, { statement: string; signature: string }>;
    recovery: { statement: string; signature: string }[];
    manifests: Map<number, string>;
    records: { cursor: number; generation: number; record_id: string; data: string }[];
    transfers: { cursor: number; transfer: string }[];
  }>();
  n = 0;

  fetch = async (url: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const path = String(url).replace(/^.*\/v1/, "");
    this.requests.push(`${init?.method ?? "GET"} ${path}`);
    const body = init?.body ? JSON.parse(init.body as string) : {};
    const auth = (init?.headers as Record<string, string> | undefined)?.Authorization?.replace("Bearer ", "");
    const handle = auth ? this.sessions.get(auth) : undefined;
    const ok = (data: unknown) => new Response(JSON.stringify(data), { status: 200 });
    const err = (status: number, error: string) => new Response(JSON.stringify({ error, message: error }), { status });
    const service = (await keyFromSeed(new Uint8Array(32).fill(42))).publicKey;
    const session = (handle: string, enrollment: boolean, device?: string, expiry = Date.now() + 300_000) => {
      const token = `token-${++this.n}`;
      this.sessions.set(token, handle);
      if (enrollment) this.enrollmentTokens.add(token);
      if (device) this.sessionDevices.set(token, device);
      this.expiries.set(token, expiry);
      return token;
    };
    if (path === "/service-key") return ok({ key: service });
    if (path === "/register") {
      if (this.accounts.has(body.handle)) return err(409, "taken");
      const d = body.device;
      if (!await verifyDeviceCertificate(body.account_key, d)) return err(400, "bad_request");
      this.accounts.set(body.handle, { ...body, credential_version: 0 });
      this.devices.push({ handle: body.handle, device_key: d.device_key, name: d.name, revoked_at: null, certificate: d });
      return ok({ token: session(body.handle, true, d.device_key) });
    }
    if (path === "/prelogin") return ok({ kdf: this.accounts.get(body.handle)?.kdf ?? newKdf() });
    if (path === "/login" || path === "/recover") {
      const a = this.accounts.get(body.handle);
      const recovery = path === "/recover";
      if (!a || (recovery ? a.recovery_auth !== body.recovery_auth : a.auth_key !== body.auth_key)) return err(401, "unauthorized");
      return ok({ token: session(body.handle, true), account_key: a.account_key, ...(recovery ? { recovery_blob: a.recovery_blob } : { key_blob: a.key_blob }) });
    }
    if (path.startsWith("/accounts/")) {
      const a = this.accounts.get(decodeURIComponent(path.slice(10)));
      return a ? ok({ handle: path.slice(10), account_key: a.account_key }) : err(404, "not_found");
    }
    if (!handle || !auth || (this.expiries.get(auth) ?? 0) <= Date.now()) return err(401, "unauthorized");
    const a = this.accounts.get(handle)!;
    const enrollment = this.enrollmentTokens.has(auth);
    if (path === "/devices" && init?.method === "POST") {
      if (!enrollment) return err(401, "unauthorized");
      if (!await verifyDeviceCertificate(a.account_key, body)) return err(400, "bad_request");
      const bound = this.sessionDevices.get(auth);
      if (bound && bound !== body.device_key) return err(400, "bad_request");
      const known = this.devices.find((d) => d.device_key === body.device_key);
      if (known?.revoked_at) return err(410, "revoked");
      if (known && known.handle !== handle) return err(409, "taken");
      const certificate = { issued_at: body.issued_at, expires_at: body.expires_at, signature: body.signature };
      if (known) {
        known.name = body.name;
        if (!known.certificate || known.certificate.expires_at < body.expires_at) known.certificate = certificate;
      } else this.devices.push({ handle, device_key: body.device_key, name: body.name, revoked_at: null, certificate });
      this.sessionDevices.set(auth, body.device_key);
      return ok({});
    }
    if (path === "/challenges") {
      if (body.purpose === "session" ? !enrollment : enrollment) return err(401, "unauthorized");
      if (!["session", "password", "recovery"].includes(body.purpose)) return err(400, "bad_request");
      if (body.purpose === "session" && (this.sessionDevices.get(auth) !== body.device?.device_key || !await verifyDeviceCertificate(a.account_key, body.device))) return err(400, "bad_request");
      const reply = { challenge: encodeBase64Url(crypto.getRandomValues(new Uint8Array(32))), expires_at: Date.now() + 300_000, credential_version: a.credential_version, service_key: service };
      this.challenges.set(reply.challenge, { token: auth, purpose: body.purpose, cert: body.device, reply });
      return ok(reply);
    }
    if (path === "/session") {
      const c = this.challenges.get(body.challenge);
      if (!enrollment || !c || c.token !== auth || c.purpose !== "session" || c.reply.expires_at <= Date.now()) return err(401, "unauthorized");
      if (await certificateHash(a.account_key, body.device) !== await certificateHash(a.account_key, c.cert!) || !await verifyText(body.device.device_key, await sessionStatement(service, a.account_key, auth, c.reply, body.device), body.signature)) return err(400, "bad_request");
      const d = this.devices.find((d) => d.device_key === body.device.device_key && d.handle === handle);
      if (!d || d.revoked_at !== null || body.device.expires_at <= Date.now()) return err(401, "unauthorized");
      this.challenges.delete(body.challenge);
      this.sessions.delete(auth);
      return ok({ token: session(handle, false, d.device_key, Math.min(Date.now() + 90 * DAY_MS, body.device.expires_at)) });
    }
    if (enrollment) return err(401, "unauthorized");
    if (path === "/account/password" || path === "/account/recovery") {
      const c = this.challenges.get(body.challenge);
      const purpose = path.endsWith("password") ? "password" : "recovery";
      if (!c || c.token !== auth || c.purpose !== purpose || c.reply.expires_at <= Date.now()) return err(401, "unauthorized");
      if (body.credential_version !== a.credential_version || c.reply.credential_version !== a.credential_version) return err(409, "conflict");
      const recovery = purpose === "recovery";
      const statement = await credentialsStatement(purpose, service, a.account_key, auth, c.reply, recovery ? null : body.kdf, recovery ? body.recovery_auth : body.auth_key, recovery ? body.recovery_blob : body.key_blob);
      if (!await verifyText(a.account_key, statement, body.signature)) return err(400, "bad_request");
      this.challenges.delete(body.challenge);
      Object.assign(a, recovery ? { recovery_auth: body.recovery_auth, recovery_blob: body.recovery_blob } : { kdf: body.kdf, auth_key: body.auth_key, key_blob: body.key_blob });
      a.credential_version++;
      for (const [t, h] of this.sessions) if (h === handle && t !== auth) this.sessions.delete(t);
      return ok({});
    }
    if (path === "/devices" && init?.method === "GET") return ok({ devices: this.devices.filter((d) => d.handle === handle).map((d) => ({ ...d, created_at: 1, last_seen: 1 })) });
    if (path === "/devices/revoke") {
      if (!await verifyText(a.account_key, revokeStatement(a.account_key, body.device_key, body.revoked_at), body.signature)) return err(400, "bad_request");
      const d = this.devices.find((d) => d.device_key === body.device_key && d.handle === handle);
      if (!d) return err(404, "not_found");
      d.revoked_at = body.revoked_at;
      for (const [t, device] of this.sessionDevices) if (device === body.device_key) this.sessions.delete(t);
      return ok({});
    }
    if (path === "/vault" && init?.method === "GET") {
      const v = this.vaults.get(handle);
      return ok({ vault: v?.vault ?? null, version: v?.version ?? 0, updated_at: 1 });
    }
    if (path === "/vault") {
      const have = this.vaults.get(handle)?.version ?? 0;
      if (body.version !== have) return err(409, "conflict");
      this.vaults.set(handle, { vault: body.vault, version: have + 1 });
      return ok({ version: have + 1 });
    }
    if (path.startsWith("/archive/")) {
      const device = this.sessionDevices.get(auth);
      if (!device) return err(401, "unauthorized");
      let ar = this.archive.get(handle);
      if (!ar) this.archive.set(handle, ar = { devices: new Map(), recovery: [], manifests: new Map(), records: [], transfers: [] });
      const [route, query] = path.slice(9).split("?");
      const after = Number(new URLSearchParams(query).get("after") ?? 0);
      const method = init?.method ?? "GET";
      if (route === "device" && method === "PUT") {
        if (body.statement.split("\n")[2] !== device) return err(400, "bad_request");
        if (ar.devices.has(device)) return err(409, "conflict");
        ar.devices.set(device, { statement: body.statement, signature: body.signature });
        return ok({});
      }
      if (route === "devices") {
        return ok({ devices: [...ar.devices].map(([key, d]) => {
          const known = this.devices.find((x) => x.device_key === key);
          return { ...d, certificate: known?.revoked_at === null ? known.certificate : null };
        }) });
      }
      if (route === "recovery" && method === "PUT") {
        const version = Number(body.statement.split("\n")[2]);
        const newest = ar.recovery.at(-1);
        if (newest && version === ar.recovery.length && newest.statement === body.statement) return ok({});
        if (version !== ar.recovery.length + 1) return err(409, "conflict");
        ar.recovery.push({ statement: body.statement, signature: body.signature });
        return ok({});
      }
      if (route === "recovery") return ok({ keys: ar.recovery });
      if (route === "manifests" && method === "POST") {
        const bytes = decodeBase64Url(body.manifest);
        const generation = Number(new TextDecoder().decode(bytes).split("\n")[2]);
        if (ar.manifests.has(generation)) return err(409, "conflict");
        ar.manifests.set(generation, body.manifest);
        return ok({});
      }
      if (route === "manifests") {
        const list = [...ar.manifests].filter(([g]) => g > after).sort(([x], [y]) => x - y).map(([generation, manifest]) => ({ generation, manifest }));
        return ok({ manifests: list.slice(0, 100), next: list.length > 100 ? list[99]!.generation : null });
      }
      if (route === "records" && method === "POST") {
        for (const r of body.records) {
          if (!ar.records.some((x) => x.record_id === r.record_id)) ar.records.push({ cursor: ++this.n, ...r });
        }
        return ok({});
      }
      if (route === "records") {
        const list = ar.records.filter((r) => r.cursor > after);
        return ok({ records: list.slice(0, 1000), next: list.length > 1000 ? list[999]!.cursor : null });
      }
      if (route === "transfers" && method === "POST") {
        ar.transfers.push({ cursor: ++this.n, transfer: body.transfer });
        return ok({});
      }
      if (route === "transfers") {
        const list = ar.transfers.filter((t) => t.cursor > after);
        return ok({ transfers: list.slice(0, 100), next: list.length > 100 ? list[99]!.cursor : null });
      }
      return err(404, "not_found");
    }
    if (path === "/logout") {
      this.sessions.delete(auth);
      return ok({});
    }
    return err(404, "not_found");
  };
}

export function setup() {
  const service = new FakeService();
  let stored: ConnectRecord | undefined;
  const store: ConnectStore = { load: async () => stored, save: async (r) => void (stored = r), clear: async () => void (stored = undefined) };
  const deps = (name = "Chrome on macOS") => ({ api: new ConnectApi("https://c", service.fetch as typeof fetch), store, deviceName: name });
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
