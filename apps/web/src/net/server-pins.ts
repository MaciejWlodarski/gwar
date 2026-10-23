import { decodeFixed, normalizeEndpoint, serverPublicKey } from "./handshake";
import { localState } from "../lib/local-state";

export interface ServerPin {
  server_pk: string;
  first_seen: number;
}

export interface PinStore {
  /** Throws when the pins cannot be read: nothing is trusted then. */
  load(endpoint: string): ServerPin | undefined;
  /** First contact commits only if the stored pin is absent or matches, in one transaction. */
  commit(endpoint: string, key: string): Promise<void>;
  /** Removes the pin if it still is `previous`. */
  forget(endpoint: string, previous: string): Promise<void>;
}

export class ServerIdentityChanged extends Error {
  constructor(readonly endpoint: string, readonly expected: string, readonly candidate: string, readonly source: "pin" | "invite") {
    super("server identity changed");
  }
}

const PREFIX = "gwar.server-pin:";

function parsePin(value: string): ServerPin {
  const pin: ServerPin = JSON.parse(value);
  decodeFixed(pin.server_pk, 32);
  if (!Number.isSafeInteger(pin.first_seen) || pin.first_seen < 0) throw new Error("invalid server pin");
  return pin;
}

/** Pins in the device's security state (lib/local-state.ts). */
export const serverPins: PinStore = {
  load(endpoint) {
    const value = localState.get(PREFIX + normalizeEndpoint(endpoint));
    return value === undefined ? undefined : parsePin(value);
  },
  async commit(endpoint, key) {
    await localState.update(PREFIX + normalizeEndpoint(endpoint), (stored) => {
      if (stored === undefined) return JSON.stringify({ server_pk: key, first_seen: Date.now() });
      const pin = parsePin(stored);
      if (pin.server_pk !== key) throw new ServerIdentityChanged(endpoint, pin.server_pk, key, "pin");
      return undefined;
    });
  },
  async forget(endpoint, previous) {
    await localState.update(PREFIX + normalizeEndpoint(endpoint), (stored) => stored !== undefined && parsePin(stored).server_pk === previous ? null : undefined);
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
