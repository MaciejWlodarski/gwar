import type { PinStore, ServerPin } from "./server-pins";
import type { Welcome } from "../proto/Welcome";
import { generateIdentity } from "./identity";
import { newNonce, proofStatement } from "./handshake";
import type { Hello } from "../proto/Hello";
import { vi } from "vitest";
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

export const serverInfo = { name: "Test", welcome: "hi", version: "0", default_channel: 1, max_clients: 10, upload_limit: 0, file_access: "link" as const, teamspeak: false };

export function challenge(passwordRequired = false) {
  return { ev: "challenge", d: { protocol: 4, server_pk: testServerIdentity.publicKey, server_nonce: newNonce(), server: serverInfo, password_required: passwordRequired } };
}

export function welcome(overrides: Partial<Welcome> = {}): Welcome {
  return {
    session: 1,
    uid: "uid1",
    server: serverInfo,
    permissions: [],
    groups: [],
    channels: [{ id: 1, parent: null, name: "Lobby", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false }],
    clients: [],
    members: [],
    unread: [],
    conversations: [],
    ice_servers: [],
    file_access: "test-file-access",
    ...overrides,
  };
}

export const fakeIdentity = await generateIdentity();
export const testServerIdentity = await generateIdentity();

export async function finishHandshake(s: FakeSocket, w = welcome()): Promise<void> {
  const hello = s.sent[0]!.d as Hello;
  const server_proof = await testServerIdentity.sign(new TextEncoder().encode(await proofStatement(hello)));
  s.push({ re: 1, ok: { server_proof } });
  await vi.waitFor(() => { if (s.sent.length < 2) throw new Error("waiting for join"); });
  s.push({ re: 2, ok: w });
}

export function memoryPins(): PinStore & { pins: Map<string, ServerPin> } {
  const pins = new Map<string, ServerPin>();
  return { pins, load: (endpoint) => pins.get(endpoint),
    commit(endpoint, server_pk) {
      const old = pins.get(endpoint);
      if (old && old.server_pk !== server_pk) throw new Error("pin changed");
      if (!old) pins.set(endpoint, { server_pk, first_seen: Date.now() });
    },
    forget(endpoint, previous) { if (pins.get(endpoint)?.server_pk === previous) pins.delete(endpoint); },
  };
}
