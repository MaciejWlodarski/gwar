declare module "*gwar_mls_wasm.js" {
  export default function init(): Promise<unknown>;
  export class Agent {
    constructor(seed: Uint8Array, credential: Uint8Array, now: number);
    static load(seed: Uint8Array, credential: Uint8Array, records: unknown): Agent;
    execute(request: unknown): unknown;
    take_changes(): import("./types").Changes;
    free(): void;
  }
  /** The envelope of a confirmer anchor signed by the owner's account key, verified. */
  export function anchor_envelope(statement: string, account_key: string, signature: string): Uint8Array;
}
