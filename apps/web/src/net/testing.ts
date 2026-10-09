import type { Welcome } from "../proto/Welcome";
import type { Identity } from "./identity";
import type { WebSocketLike } from "./connection";

/** In-memory WebSocket for tests; the "server" side is driven by hand. */
export class FakeSocket implements WebSocketLike {
  static instances: FakeSocket[] = [];
  readyState = 0;
  sent: Array<Record<string, unknown>> = [];
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.instances.push(this);
  }

  static reset(): void {
    FakeSocket.instances = [];
  }

  static last(): FakeSocket {
    const s = FakeSocket.instances[FakeSocket.instances.length - 1];
    if (!s) throw new Error("no socket");
    return s;
  }

  send(data: string): void {
    this.sent.push(JSON.parse(data) as Record<string, unknown>);
  }

  close(): void {
    this.readyState = 3;
  }

  // server side helpers
  open(): void {
    this.readyState = 1;
    this.onopen?.({});
  }

  push(frame: unknown): void {
    this.onmessage?.({ data: JSON.stringify(frame) });
  }

  drop(): void {
    this.readyState = 3;
    this.onclose?.({ code: 1006, reason: "" });
  }
}

export const serverInfo = { name: "Test", welcome: "hi", version: "0", default_channel: 1, max_clients: 10 };

export function challenge(passwordRequired = false) {
  return { ev: "challenge", d: { protocol: 1, nonce: "abc", server: serverInfo, password_required: passwordRequired } };
}

export function welcome(overrides: Partial<Welcome> = {}): Welcome {
  return {
    session: 1,
    uid: "uid1",
    server: serverInfo,
    permissions: [],
    groups: [],
    channels: [{ id: 1, parent: null, name: "Lobby", topic: "", position: 0, has_password: false, max_clients: null }],
    clients: [],
    members: [],
    unread: [],
    ice_servers: [],
    ...overrides,
  };
}

export const fakeIdentity: Identity = {
  publicKey: "PUBKEY",
  sign: async (m) => `sig(${new TextDecoder().decode(m).replace(/\n/g, "|")})`,
  exportBackup: () => ({ format: "vc-identity", version: 1, jwk: {} }),
};
