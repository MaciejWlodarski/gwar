import type { AgentFactory } from "./types";
let modulePromise: Promise<typeof import("../../../../crates/gwar-mls-wasm/pkg/gwar_mls_wasm.js")> | undefined;
/** No wasm fetch/compilation until the first MLS session. Failed loads can retry. */
/** A volatile voice agent: created per call, never persisted. */
export async function loadVoiceAgent(seed: Uint8Array, credential: Uint8Array, now: () => number): Promise<AgentFactory> {
  await loadAgent(seed, credential, now);
  const { Agent } = await modulePromise!;
  return { create: () => Agent.new_voice(seed, credential, now()), load: records => Agent.load_voice(seed, credential, records) };
}
/** A confirmer anchor's envelope from its statement and the owner's account signature (6b). */
export async function anchorEnvelope(statement: string, accountKey: string, signature: string): Promise<Uint8Array> {
  if (!modulePromise) throw new Error("MLS module not loaded");
  return (await modulePromise).anchor_envelope(statement, accountKey, signature);
}
export async function loadAgent(seed: Uint8Array, credential: Uint8Array, now: () => number): Promise<AgentFactory> {
  modulePromise ??= import("../../../../crates/gwar-mls-wasm/pkg/gwar_mls_wasm.js").then(async module => { await module.default(); return module; }).catch(error => { modulePromise = undefined; throw error; });
  const { Agent } = await modulePromise;
  return { create: () => new Agent(seed, credential, now()), load: records => Agent.load(seed, credential, records) };
}
