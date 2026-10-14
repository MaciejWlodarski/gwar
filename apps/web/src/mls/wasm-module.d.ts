declare module "*gwar_mls_wasm.js" {
  export default function init(): Promise<unknown>;
  export class Agent {
    constructor(seed: Uint8Array, credential: Uint8Array, now: number);
    static load(seed: Uint8Array, credential: Uint8Array, records: unknown): Agent;
    execute(request: unknown): unknown;
    take_changes(): import("./types").Changes;
    free(): void;
  }
}
