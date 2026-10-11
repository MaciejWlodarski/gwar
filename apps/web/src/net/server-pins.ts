import { decodeFixed, normalizeEndpoint, serverPublicKey } from "./handshake";

export interface ServerPin {
  server_pk: string;
  first_seen: number;
}

export interface PinStore {
  load(endpoint: string): ServerPin | undefined;
  /** First contact commits only if the concurrent stored pin is absent or matches. */
  commit(endpoint: string, key: string): void;
  forget(endpoint: string, previous: string): void;
}

export class ServerIdentityChanged extends Error {
  constructor(readonly endpoint: string, readonly expected: string, readonly candidate: string, readonly source: "pin" | "invite") {
    super("server identity changed");
  }
}

const PREFIX = "vc.server-pin:";
export const serverPins: PinStore = {
  load(endpoint) {
    const value = localStorage.getItem(PREFIX + normalizeEndpoint(endpoint));
    if (value === null) return undefined;
    const pin: ServerPin = JSON.parse(value);
    decodeFixed(pin.server_pk, 32);
    if (!Number.isSafeInteger(pin.first_seen) || pin.first_seen < 0) throw new Error("invalid server pin");
    return pin;
  },
  commit(endpoint, key) {
    const old = this.load(endpoint);
    if (old && old.server_pk !== key) throw new ServerIdentityChanged(endpoint, old.server_pk, key, "pin");
    if (!old) localStorage.setItem(PREFIX + normalizeEndpoint(endpoint), JSON.stringify({ server_pk: key, first_seen: Date.now() }));
  },
  forget(endpoint, previous) {
    if (this.load(endpoint)?.server_pk === previous) localStorage.removeItem(PREFIX + normalizeEndpoint(endpoint));
  },
};

export function checkCandidate(endpoint: string, candidate: string, store: PinStore, inviteKey?: string): void {
  serverPublicKey(candidate);
  if (inviteKey !== undefined) {
    decodeFixed(inviteKey, 32);
    if (inviteKey !== candidate) throw new ServerIdentityChanged(endpoint, inviteKey, candidate, "invite");
  }
  const pin = store.load(endpoint);
  if (pin && pin.server_pk !== candidate) throw new ServerIdentityChanged(endpoint, pin.server_pk, candidate, "pin");
}
