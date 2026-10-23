import { decodeFixed, normalizeEndpoint, serverPublicKey } from "./handshake";
import { localState } from "../lib/local-state";

export interface ServerPin {
  server_pk: string;
  first_seen: number;
}

export interface PinStore {
  /** Throws when the pins cannot be read: nothing is trusted then. */
  load(endpoint: string): ServerPin | undefined;
  /**
   * After the server proved `key`, in one transaction: first contact commits only if no pin is stored, and
   * a stored pin must match. With `replacing` (the user accepted `key` in place of that key) a pin still
   * holding `replacing` is replaced by `key`. A pin is never removed: the address is never without one.
   */
  commit(endpoint: string, key: string, replacing?: string): Promise<void>;
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
  async commit(endpoint, key, replacing) {
    await localState.update(PREFIX + normalizeEndpoint(endpoint), (stored) => {
      const pin = stored === undefined ? undefined : parsePin(stored);
      if (pin?.server_pk === key) return undefined;
      if (pin === undefined || (replacing !== undefined && pin.server_pk === replacing)) return JSON.stringify({ server_pk: key, first_seen: Date.now() });
      throw new ServerIdentityChanged(endpoint, pin.server_pk, key, "pin");
    });
  },
};

/** Before the hello: `candidate` must match the invite's key and the pin (or be the accepted replacement of it). */
export function checkCandidate(endpoint: string, candidate: string, store: PinStore, inviteKey?: string, replacing?: string): void {
  serverPublicKey(candidate);
  if (inviteKey !== undefined) {
    decodeFixed(inviteKey, 32);
    if (inviteKey !== candidate) throw new ServerIdentityChanged(endpoint, inviteKey, candidate, "invite");
  }
  const pin = store.load(endpoint);
  if (pin && pin.server_pk !== candidate && !(replacing !== undefined && pin.server_pk === replacing)) throw new ServerIdentityChanged(endpoint, pin.server_pk, candidate, "pin");
}
