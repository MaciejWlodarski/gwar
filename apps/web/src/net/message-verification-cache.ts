import type { ChatMessage } from "../proto/ChatMessage";
import { confirmedBefore, messageStorage, refreshRevocations, MAX_CHECKPOINTS, type MessageStorage, type RevocationState } from "./message-storage";
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
  /** Refresh time/feed qualifications without hashing bodies or checking signatures again; only the
   *  checkpoints storage could not take before are taken now. */
  async refresh(now = Date.now()): Promise<void> {
    const entries = [...this.entries.values()];
    const results = await Promise.all(entries.map((e) => e.state));
    await Promise.all(entries.map(async (entry, i) => {
      if (results[i]!.status !== "verified" || results[i]!.continuity !== "storage-unavailable") return;
      entry.state = this.verifier.checkpoint(entry.msg, entry.ctx, results[i]!);
      results[i] = await entry.state;
    }));
    const certified = entries.filter((e, i) => results[i]!.status === "verified" && e.msg.signature?.certificate);
    if (!certified.length) return;
    await refreshRevocations();
    const revocations = await this.storage.revocations(certified.map((e) => ({
      account: e.msg.signature!.certificate!.account_key, device: e.msg.signature!.certificate!.device_key,
    })), undefined, now).catch(() => new Map<string, RevocationState>());
    entries.forEach((entry, i) => {
      const old = results[i]!, signature = entry.msg.signature, certificate = signature?.certificate;
      if (old.status !== "verified" || !certificate) return;
      const state: MessageVerification = { ...old, authorization: [] };
      const revocation = revocations.get(`${certificate.account_key}:${certificate.device_key}`);
      if (revocation?.revokedAt !== undefined) {
        if (signature.signed_at >= revocation.revokedAt) { entry.state = Promise.resolve(invalidVerification()); return; }
        state.authorization.push("device-revoked");
      }
      if (!revocation?.fresh && !confirmedBefore(old.order, revocation)) state.authorization.push("revocations-stale");
      if (certificate.expires_at <= now) state.authorization.push("certificate-expired");
      entry.state = Promise.resolve(state);
    });
  }
}
