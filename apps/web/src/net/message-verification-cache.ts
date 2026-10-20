import type { ChatMessage } from "../proto/ChatMessage";
import { confirmedBefore, messageStorage, refreshRevocations, MAX_CHECKPOINTS, type MessageStorage } from "./message-storage";
import { messageFingerprint, MessageVerifier, invalidVerification, type MessageContext, type MessageVerification } from "./message-signature";

/** View-local cache: immutable message/context fingerprints also share in-flight verification. */
export class MessageVerificationCache {
  private entries = new Map<number, { fingerprint: string; msg: ChatMessage; ctx: MessageContext; state: Promise<MessageVerification> }>();
  private verifier: MessageVerifier;
  constructor(private storage: MessageStorage = messageStorage) { this.verifier = new MessageVerifier(storage); }
  verify(msg: ChatMessage, ctx: MessageContext): Promise<MessageVerification> {
    const fingerprint = messageFingerprint(msg, ctx), old = this.entries.get(msg.id);
    if (old?.fingerprint === fingerprint) return old.state;
    const snapshot = structuredClone(msg), context = structuredClone(ctx), state = this.verifier.verify(snapshot, context);
    this.entries.set(msg.id, { fingerprint, msg: snapshot, ctx: context, state });
    if (this.entries.size > MAX_CHECKPOINTS) this.entries.delete(this.entries.keys().next().value!);
    return state;
  }
  private running?: Promise<void>;
  private queued?: { now: number; done: Promise<void> };
  /** Refresh time/feed qualifications without hashing bodies or checking signatures again; only the
   *  checkpoints storage could not take before are taken now. One refresh at a time, at most one more
   *  waiting (at the latest time asked for), so none acts on results another changed meanwhile. */
  refresh(now = Date.now()): Promise<void> {
    if (this.queued) { this.queued.now = now; return this.queued.done; }
    if (!this.running) return this.start(now);
    const queued: { now: number; done?: Promise<void> } = { now };
    queued.done = this.running.then(() => { this.queued = undefined; return this.start(queued.now); });
    this.queued = queued as { now: number; done: Promise<void> };
    return queued.done;
  }
  private start(now: number): Promise<void> {
    const run: Promise<void> = this.run(now).catch(() => {}).finally(() => { if (this.running === run) this.running = undefined; });
    return this.running = run;
  }
  private async run(now: number): Promise<void> {
    const entries = [...this.entries.values()];
    await Promise.all(entries.map(async (entry) => {
      const current = await entry.state;
      if (current.status !== "verified" || current.continuity !== "storage-unavailable") return;
      await (entry.state = this.verifier.checkpoint(entry.msg, entry.ctx, current));
    }));
    const certified: typeof entries = [];
    for (const e of entries) if ((await e.state).status === "verified" && e.msg.signature?.certificate) certified.push(e);
    if (!certified.length) return;
    await refreshRevocations();
    const revocations = await this.storage.revocations(certified.map((e) => ({
      account: e.msg.signature!.certificate!.account_key, device: e.msg.signature!.certificate!.device_key,
    })), undefined, now).catch(() => undefined);
    // From each result as it is now, never as it was before the awaits above.
    const results = await Promise.all(entries.map((e) => e.state));
    entries.forEach((entry, i) => {
      const old = results[i]!, signature = entry.msg.signature, certificate = signature?.certificate;
      if (old.status !== "verified" || !certificate) return;
      if (!revocations) {
        // Unreadable now: what was known stays (a revocation above all), qualified as stale.
        const notes = new Set([...old.authorization, "revocations-stale" as const]);
        if (certificate.expires_at <= now) notes.add("certificate-expired");
        entry.state = Promise.resolve({ ...old, authorization: [...notes] });
        return;
      }
      const state: MessageVerification = { ...old, authorization: [] };
      const revocation = revocations.get(`${certificate.account_key}:${certificate.device_key}`);
      if (revocation?.revokedAt !== undefined) {
        if (signature.signed_at >= revocation.revokedAt) { entry.state = Promise.resolve(invalidVerification()); return; }
        state.authorization.push("device-revoked");
      } else if (old.authorization.includes("device-revoked")) state.authorization.push("device-revoked"); // Stored revocations are never dropped; neither is a known one.
      if (!revocation?.fresh && !confirmedBefore(old.order, revocation)) state.authorization.push("revocations-stale");
      if (certificate.expires_at <= now) state.authorization.push("certificate-expired");
      entry.state = Promise.resolve(state);
    });
  }
}
