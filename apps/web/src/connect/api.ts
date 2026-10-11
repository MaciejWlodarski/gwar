/** HTTP client for the Gwar Connect service (docs/connect.md, "Connect API"). */
import { connectJson } from "./transport";
import { CONNECT_URL } from "../lib/official";
import type { ChallengePurpose, ConnectChallenge, Kdf, SignedDevice } from "./crypto";

export type ConnectErrorKind =
  | "network" // could not reach the service
  | "unauthorized" // wrong handle or password / recovery code, or the session ended
  | "rate_limited"
  | "taken" // handle or identity already has an account, or the device key is used
  | "revoked"
  | "not_found"
  | "conflict" // the vault changed since it was read
  | "bad_request"
  | "server"; // anything else

export class ConnectApiError extends Error {
  constructor(
    readonly kind: ConnectErrorKind,
    message: string,
    readonly status = 0,
  ) {
    super(message);
    this.name = "ConnectApiError";
  }
}

const KIND_BY_CODE: Record<string, ConnectErrorKind> = {
  unauthorized: "unauthorized",
  rate_limited: "rate_limited",
  taken: "taken",
  revoked: "revoked",
  not_found: "not_found",
  conflict: "conflict",
  bad_request: "bad_request",
};

export interface DeviceCertBody extends SignedDevice {
  name: string;
}

export interface RegisterBody {
  handle: string;
  account_key: string;
  kdf: Kdf;
  auth_key: string;
  key_blob: string;
  recovery_auth: string;
  recovery_blob: string;
  device: DeviceCertBody;
}

export interface CredentialAuthorization {
  challenge: string;
  credential_version: number;
  signature: string;
}

export interface PasswordBody extends CredentialAuthorization {
  kdf: Kdf;
  auth_key: string;
  key_blob: string;
}

export interface RecoveryBody extends CredentialAuthorization {
  recovery_auth: string;
  recovery_blob: string;
}

export interface LoginReply {
  token: string;
  account_key: string;
  key_blob: string;
}

export interface RecoverReply {
  token: string;
  account_key: string;
  recovery_blob: string;
}

export interface AccountInfo {
  handle: string;
  account_key: string;
  created_at: number;
}

export interface DeviceInfo {
  device_key: string;
  name: string;
  created_at: number;
  last_seen: number;
  revoked_at: number | null;
  /** The newest certificate Connect has for this device; null for a device that never had one. */
  certificate: { issued_at: number; expires_at: number; signature: string } | null;
}

export interface Revocation {
  seq: number;
  account_key: string;
  device_key: string;
  revoked_at: number;
  signature: string;
}

export interface VaultReply {
  /** The sealed vault, or null before the first write. */
  vault: string | null;
  /** 0 before the first write; the next write names the version it was based on. */
  version: number;
  updated_at?: number;
}

export class ConnectApi {
  constructor(
    private readonly base: string = CONNECT_URL,
    private readonly fetchFn: typeof fetch = (...args) => fetch(...args),
  ) {}

  private async call<T>(method: "GET" | "POST" | "PUT", path: string, body?: unknown, token?: string): Promise<T> {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (token) headers.Authorization = `Bearer ${token}`;
    let response: Response;
    let data: unknown;
    try {
      ({ response, data } = await connectJson(this.base, `/v1${path}`, {
        method, headers, body: body === undefined ? undefined : JSON.stringify(body),
      }, this.fetchFn));
    } catch (e) {
      throw new ConnectApiError("network", e instanceof Error ? e.message : String(e));
    }
    if (!response.ok) {
      const err = (data ?? {}) as { error?: string; message?: string };
      const kind = (err.error && KIND_BY_CODE[err.error]) || (response.status === 429 ? "rate_limited" : "server");
      throw new ConnectApiError(kind, err.message ?? `HTTP ${response.status}`, response.status);
    }
    return data as T;
  }

  register(body: RegisterBody): Promise<{ token: string }> {
    return this.call("POST", "/register", body);
  }

  async prelogin(handle: string): Promise<Kdf> {
    return (await this.call<{ kdf: Kdf }>("POST", "/prelogin", { handle })).kdf;
  }

  login(handle: string, authKey: string): Promise<LoginReply> {
    return this.call("POST", "/login", { handle, auth_key: authKey });
  }

  recover(handle: string, recoveryAuth: string): Promise<RecoverReply> {
    return this.call("POST", "/recover", { handle, recovery_auth: recoveryAuth });
  }

  account(token: string): Promise<AccountInfo> {
    return this.call("GET", "/account", undefined, token);
  }

  async serviceKey(): Promise<string> {
    return (await this.call<{ key: string }>("GET", "/service-key")).key;
  }

  challenge(token: string, purpose: ChallengePurpose, device?: SignedDevice): Promise<ConnectChallenge> {
    return this.call("POST", "/challenges", { purpose, device }, token);
  }

  async session(token: string, challenge: string, device: SignedDevice, signature: string): Promise<string> {
    return (await this.call<{ token: string }>("POST", "/session", { challenge, device, signature }, token)).token;
  }

  async changePassword(token: string, body: PasswordBody): Promise<void> {
    await this.call("PUT", "/account/password", body, token);
  }

  async changeRecovery(token: string, body: RecoveryBody): Promise<void> {
    await this.call("PUT", "/account/recovery", body, token);
  }

  async devices(token: string): Promise<DeviceInfo[]> {
    return (await this.call<{ devices: DeviceInfo[] }>("GET", "/devices", undefined, token)).devices;
  }

  async addDevice(token: string, device: DeviceCertBody): Promise<void> {
    await this.call("POST", "/devices", device, token);
  }

  async revokeDevice(token: string, revocation: { device_key: string; revoked_at: number; signature: string }): Promise<void> {
    await this.call("POST", "/devices/revoke", revocation, token);
  }

  getVault(token: string): Promise<VaultReply> {
    return this.call("GET", "/vault", undefined, token);
  }

  /** Writes the vault on top of `version`; rejects with kind "conflict" if it changed meanwhile. Returns the new version. */
  async putVault(token: string, vault: string, version: number): Promise<number> {
    return (await this.call<{ version: number }>("PUT", "/vault", { vault, version }, token)).version;
  }

  async logout(token: string): Promise<void> {
    await this.call("POST", "/logout", undefined, token);
  }

  async revocations(since = 0): Promise<Revocation[]> {
    return (await this.call<{ revocations: Revocation[] }>("GET", `/revocations?since=${since}`)).revocations;
  }

  /** The public account key of a handle (needed to derive recovery secrets). */
  async publicAccount(handle: string): Promise<{ handle: string; account_key: string }> {
    return this.call("GET", `/accounts/${encodeURIComponent(handle)}`);
  }
}

export const connectApi = new ConnectApi();
