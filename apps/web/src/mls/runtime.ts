import type { Link } from "../net/connection";
import { RequestError } from "../net/connection";
import type { Identity } from "../net/identity";
import { uidForPublicKey } from "../net/identity";
import { decodeFixed } from "../net/handshake";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { Op, RequestInput, ResponseMap } from "../net/protocol";
import type { MlsGroup } from "../proto/MlsGroup";
import type { MlsDevice } from "../proto/MlsDevice";
import { credential, descriptor, participants } from "./descriptor";
import { AgentOperationError, Engine, acquireDeviceLock, jobChange, jobKey } from "./engine";
import { openStorage, type Persistence } from "./storage";
import { loadAgent } from "./wasm";
import type { AgentFactory, Change, CommitOutbox, ConversationStatus, Gate, GroupSummary, HistoryJob, OpenResult, PackageJob, Plaintext, Resolution, Revocation, SealedFields } from "./types";
const bytes = (value: string): number[] => [...decodeBase64Url(value)];
const encoded = (value: number[]): string => encodeBase64Url(new Uint8Array(value));
/** The DS protocol currently uses safe JSON integers; core epochs remain u64 strings. */
function wireEpoch(value: string | number): number {
  const n = Number(value);
  if (!Number.isSafeInteger(n) || n < 0 || String(n) !== String(value)) throw new Error("epoch outside delivery protocol range");
  return n;
}
export interface SealContext { server_key: string; conversation: string; group_id: string; epoch: string }
export interface RuntimeOptions {
  identity: Identity;
  /** Ed25519 JWK d, supplied by the identity owner; never stored again here. */
  seed: Uint8Array;
  /** A new, unconnected Gwar link. The runtime owns connect/close and the lock. */
  link: Link;
  revocations: () => Revocation[];
  onError?: (error: unknown) => void;
  now?: () => number;
  signal?: AbortSignal;
  locks?: LockManager;
  storage?: () => Promise<Persistence>;
  factory?: AgentFactory;
}
/** UI-independent MLS session. No messages are sent by this class. */
export class MlsRuntime {
  private server = "";
  private uid = "";
  private generation = 0;
  private activeGeneration = 0;
  private closePromise: Promise<void> | undefined;
  private stopped = false;
  private syncing: Promise<void> | undefined;
  private dirty = false;
  private lastPublish = -Infinity;
  private requestTokens = 12;
  private refilled = performance.now();
  private retry: ReturnType<typeof setTimeout> | undefined;
  private unsubscribers: (() => void)[] = [];
  private readonly groupVersions = new Map<string, number>();
  private readonly syncedVersions = new Map<string, number>();
  private readonly statuses = new Map<string, ConversationStatus>();
  private readonly listeners = new Set<() => void>();
  private readonly missing = new Map<string, Set<string>>();
  private readonly lastRequest = new Map<string, number>();
  private readonly now: () => number;
  private constructor(private readonly options: RuntimeOptions, private readonly engine: Engine, private readonly release: () => Promise<void>) { this.now = options.now ?? Date.now; }

  static async connect(options: RuntimeOptions): Promise<MlsRuntime> {
    if (options.link.status.state !== "idle") throw new Error("MLS must acquire its lock before connecting");
    const locks = options.locks ?? globalThis.navigator?.locks;
    if (!locks) throw new Error("Web Locks are required for MLS");
    const release = await acquireDeviceLock(options.identity.publicKey, locks, options.signal);
    let runtime: MlsRuntime | undefined;
    let storage: Persistence | undefined;
    try {
      storage = await (options.storage?.() ?? openStorage(options.identity.publicKey));
      const factory = options.factory ?? await loadAgent(options.seed, credential(options.identity), options.now ?? Date.now);
      runtime = new MlsRuntime(options, new Engine(factory, storage), release);
      runtime.uid = await uidForPublicKey(options.identity.device?.account_key ?? options.identity.publicKey);
      await runtime.engine.run(async () => {}); // Initial device record durable before connect.
      if (options.signal) {
        const abort = () => { void runtime!.close(); };
        options.signal.addEventListener("abort", abort, { once: true });
        runtime.unsubscribers.push(() => options.signal!.removeEventListener("abort", abort));
        if (options.signal.aborted) throw options.signal.reason;
      }
      runtime.unsubscribers.push(options.link.onStatus(status => {
        if (status.state !== "online") {
          runtime!.generation++;
          runtime!.lastPublish = -Infinity;
          runtime!.syncedVersions.clear();
          runtime!.markSecuring();
          if (status.state === "closed") void runtime!.close();
        }
      }));
      runtime.unsubscribers.push(options.link.onWelcome(() => {
        const server = options.link.serverKey;
        if (!server || (runtime!.server && runtime!.server !== server)) {
          options.onError?.(new Error("MLS pinned server changed"));
          void runtime!.close(); return;
        }
        runtime!.server = server;
        runtime!.schedule();
      }));
      runtime.unsubscribers.push(options.link.onEvent(event => {
        if (!event.ev.startsWith("mls.")) return;
        if (event.ev === "mls.commit" || event.ev === "mls.proposal") {
          const group = event.d.group;
          runtime!.groupVersions.set(group, (runtime!.groupVersions.get(group) ?? 0) + 1);
        }
        // Notifications are hints: replay the authoritative log (with referenced
        // proposals), never merge an event that may race an in-flight response.
        runtime!.markSecuring(); runtime!.schedule();
      }));
      await options.link.connect();
      runtime.server = options.link.serverKey ?? "";
      decodeFixed(runtime.server, 32);
      try { await runtime.sync(); }
      catch (error) {
        if (runtime.stopped) throw error;
        // A live connection remains usable while synchronization retries its
        // durable outbox. Startup/storage/handshake failures still close below.
        options.onError?.(error);
      }
      return runtime;
    } catch (error) {
      if (runtime) await runtime.close();
      else { storage?.close(); await release(); }
      throw error;
    }
  }
  status(conversation: string): ConversationStatus { return this.statuses.get(conversation) ?? "securing"; }
  subscribe(listener: () => void): () => void { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  private notify(): void { for (const listener of this.listeners) listener(); }
  private markSecuring(): void { for (const key of this.statuses.keys()) this.statuses.set(key, "securing"); this.notify(); }
  private assertCurrent(generation: number): void {
    if (this.stopped || generation !== this.generation || !this.options.link.isOnline || this.options.link.serverKey !== this.server) throw new Error("MLS connection changed");
  }
  private async request<O extends Op>(op: O, data: RequestInput<O>, guard?: () => void): Promise<ResponseMap[O]> {
    const generation = this.activeGeneration; this.assertCurrent(generation);
    // Leave room for chat/voice/control traffic in the connection's 20/s bucket.
    const now = performance.now();
    this.requestTokens = Math.min(12, this.requestTokens + (now - this.refilled) / 100);
    this.refilled = now;
    if (this.requestTokens < 1) {
      await new Promise(resolve => setTimeout(resolve, (1 - this.requestTokens) * 100));
      this.assertCurrent(generation);
      this.refilled = performance.now(); this.requestTokens = 1;
    }
    this.requestTokens--;
    guard?.();
    const result = await this.options.link.request(op, data);
    this.assertCurrent(generation); return result;
  }
  private schedule(delay = 0): void {
    if (this.stopped) return;
    if (delay === 0 && this.syncing) { this.dirty = true; return; }
    clearTimeout(this.retry);
    this.retry = setTimeout(() => { this.retry = undefined; void this.sync().catch(error => this.options.onError?.(error)); }, delay);
  }
  sync(): Promise<void> {
    if (this.syncing) { this.dirty = true; return this.syncing; }
    let failed = false;
    this.syncing = this.engine.run(async () => {
      // Bound each pass; events arriving during it request one subsequent pass.
      this.dirty = false;
      this.activeGeneration = this.generation;
      this.assertCurrent(this.generation);
      await this.synchronize();
      this.updateStatuses();
    }).catch(error => { failed = true; this.markSecuring(); throw error; }).finally(() => {
      this.syncing = undefined;
      // Periodic refill, revocation/expiry checks and missing history retries.
      this.schedule(!failed && this.dirty ? 200 : 15_000);
    });
    return this.syncing;
  }
  private inventory(): GroupSummary[] { return this.engine.query({ inventory: { server: this.server } }); }
  private summary(group: string): GroupSummary | undefined { return this.inventory().find(g => encoded(g.group) === group); }
  private args(group: string) { return { server: this.server, group: [...decodeFixed(group, 32)] }; }
  private gate(group: string): Gate { return this.engine.query({ gate: { ...this.args(group), now: this.now(), revocations: this.options.revocations() } }); }
  private outbox(group: string): CommitOutbox | null { return this.engine.query({ outbox: this.args(group) }); }
  private updateStatuses(): void {
    for (const g of this.inventory()) {
      const gate = this.gate(encoded(g.group));
      this.statuses.set(g.conversation, gate.status === "creating" ? "creating" : gate.status === "active" && !gate.rejoin && !gate.server_pending && !gate.blocked.length && !gate.pending_commit ? "ready" : "securing");
    }
    this.notify();
  }
  private historyIntents(before: GroupSummary[]): Change[] {
    const changes: Change[] = [];
    for (const g of this.inventory()) {
      if (!g.history || g.record.status !== "active") continue;
      const old = before.find(b => encoded(b.group) === encoded(g.group) && b.record.status === "active");
      for (const member of g.members) {
        if (member.device_key === this.options.identity.publicKey || old?.members.some(m => m.device_key === member.device_key)) continue;
        const job: HistoryJob = { kind: "history", server: this.server, group: encoded(g.group), recipient: member.device_key, chunks: null };
        if (!this.engine.jobs.has(jobKey(job))) changes.push(jobChange(job));
      }
    }
    return changes;
  }
  private async transition<T>(request: unknown, validate?: (result: T) => void): Promise<T> {
    const before = this.inventory();
    return this.engine.op<T>(request, result => { validate?.(result); return this.historyIntents(before); });
  }
  private async groups(): Promise<MlsGroup[]> {
    const all: MlsGroup[] = []; let after: string | null = null;
    do { const page: ResponseMap["mls.groups"] = await this.request("mls.groups", { after }); all.push(...page.groups); after = page.next; } while (after);
    return all.filter(g => g.conversation.startsWith("dm:"));
  }
  private async publish(): Promise<void> {
    if (this.now() - this.lastPublish < 60_000 && !this.engine.jobs.has(`@runtime/packages/${this.server}`)) return;
    const hpke = this.engine.query<{ hpke_key: string; created_at: number; signature: string }>({ device_hpke_key: null });
    await this.request("mls.device.publish", { hpke_key: hpke.hpke_key, created_at: hpke.created_at, signature: hpke.signature });
    const key = `@runtime/packages/${this.server}`;
    let job = this.engine.jobs.get(key) as PackageJob | undefined;
    if (job && job.expires_at <= this.now()) {
      await this.engine.saveJobs([jobChange(job, true)]); job = undefined;
    }
    if (!job) {
      const { count } = await this.request("mls.key_packages.publish", { packages: [], last_resort: null });
      // Five regular packages, plus a last-resort package. The server prunes
      // expired packages before counting; refreshing here also covers renewal.
      if (count >= 5) { this.lastPublish = this.now(); return; }
      const packages = await this.engine.op<number[][]>({ key_packages: { count: 5 - count, now: this.now() } });
      await this.engine.op<number[]>({ last_resort_key_package: { now: this.now() } }, last => {
        job = { kind: "packages", server: this.server, packages: packages.map(encoded), last_resort: encoded(last), expires_at: Math.min(this.now() + 84 * 86400_000, this.options.identity.device?.expires_at ?? Infinity) - 15 * 60_000 };
        return [jobChange(job)];
      });
    }
    await this.request("mls.key_packages.publish", { packages: job!.packages, last_resort: job!.last_resort });
    await this.engine.saveJobs([jobChange(job!, true)]);
    this.lastPublish = this.now();
  }
  private async welcomes(): Promise<void> {
    let after: string | null = null;
    do {
      const page: ResponseMap["mls.welcomes"] = await this.request("mls.welcomes", { after });
      for (const welcome of page.welcomes) {
        this.lastPublish = -Infinity;
        // Resolve an in-flight external join before handling a Welcome that may
        // have won its epoch. A durable active membership supersedes any older
        // Welcome for this group; never replace it with an older epoch.
        if (this.outbox(welcome.group)) await this.syncGroup(welcome.group);
        if (this.summary(welcome.group)?.record.status === "active" && !this.summary(welcome.group)?.record.welcome_ids?.includes(welcome.id)) {
          await this.request("mls.welcomes.ack", { ids: [welcome.id] });
          continue;
        }
        try {
          await this.transition<{ group: number[]; duplicate: boolean }>({ welcome: { server: this.server, id: welcome.id, bytes: bytes(welcome.welcome) } }, result => {
            if (encoded(result.group) !== welcome.group) throw new Error("Welcome group mismatch");
          });
        } catch (error) {
          if (!(error instanceof AgentOperationError)) throw error;
          await this.engine.reload(); this.options.onError?.(error); continue;
        }
        await this.request("mls.welcomes.ack", { ids: [welcome.id] });
      }
      after = page.next;
    } while (after);
  }
  private async synchronize(): Promise<void> {
    await this.publish();
    const groups = await this.groups();
    for (const g of this.inventory()) if (g.record.status === "creating") {
      const winner = groups.find(remote => remote.conversation === g.conversation);
      await this.transition({ settle: { ...this.args(encoded(g.group)), winner: winner ? bytes(winner.group) : null } });
    }
    await this.welcomes();
    for (const g of groups) await this.syncGroup(g.group, g.pending);
    await this.importHistory();
    let after: string | null = null;
    do {
      const page: ResponseMap["mls.history.wanted"] = await this.request("mls.history.wanted", { after });
      for (const wanted of page.wanted) await this.queueHistory(wanted.group, wanted.device);
      after = page.next;
    } while (after);
    await this.sendHistory();
    await this.requestMissing();
  }
  private async join(group: string): Promise<void> {
    const info = await this.request("mls.group_info", { group });
    await this.engine.op<CommitOutbox>({ join_external: { server: this.server, bytes: bytes(info.group_info), now: this.now(), revocations: this.options.revocations() } }, out => {
      if (encoded(out.group) !== group) throw new Error("GroupInfo group mismatch");
      return [];
    });
  }
  private async syncGroup(group: string, pending = false): Promise<void> {
    // Concurrent joins can lose. Bounded retries yield to backoff instead of
    // holding the device's queue forever on an adversarial/busy server.
    let pages = 0;
    groupLoop: for (let attempt = 0; attempt < 8; attempt++) {
      if (++pages > 128) throw new Error("MLS catch-up continues on the next pass");
      const version = this.groupVersions.get(group) ?? 0;
      let local = this.summary(group);
      if (!local || local.record.status === "removed" || local.record.status === "rejoin") { await this.join(group); local = this.summary(group)!; }
      if (local.record.status === "creating") return;
      const out = this.outbox(group);
      const since = wireEpoch(out?.epoch ?? local.record.checkpoint!.epoch);
      const log = await this.request("mls.log", { group, since, limit: 100 });
      if (log.oldest > since) {
        if (out) await this.transition({ reconcile: { ...this.args(group), result: { case: "pruned", oldest: String(log.oldest) } } });
        else await this.join(group); // Validated newer GroupInfo replaces pruned membership.
        continue;
      }
      if (out) {
        const entry = log.entries.find(e => e.epoch === since);
        if (entry) {
          try {
            await this.transition<Resolution>({ reconcile: { ...this.args(group), result: { case: "entry", epoch: String(entry.epoch), commit: bytes(entry.commit), proposals: entry.proposals.map(bytes) } } });
          } catch (error) {
            if (!(error instanceof AgentOperationError)) throw error;
            await this.engine.reload();
            // The core permits replacing this outbox only with a validated
            // GroupInfo strictly newer than its source epoch.
            await this.join(group);
          }
          continue;
        }
        if (log.head !== since) throw new Error("MLS log omitted pending commit epoch");
        await this.engine.op({ reconcile: { ...this.args(group), result: { case: "head", epoch: String(log.head) } } });
        try {
          // Never merge a success response by itself: fetch the authoritative
          // entry, which is also the path used after a lost response/reconnect.
          await this.request("mls.commit", { group, epoch: since, commit: encoded(out.commit), group_info: encoded(out.group_info), welcome: out.welcome ? encoded(out.welcome) : null });
        } catch (error) {
          if (!(error instanceof RequestError)) throw error;
          const reason = error.code === "expired" ? "expired" : error.code === "early" ? "early" : error.code === "stale" ? "stale" : error.code === "bad_request" ? "bad_request" : "other";
          const resolution = await this.engine.op<Resolution>({ reconcile: { ...this.args(group), result: { case: "rejected", reason } } });
          if (resolution === "retry") throw error;
        }
        continue;
      }
      let expected = since;
      for (const entry of log.entries) {
        if (entry.epoch !== expected) throw new Error("noncontiguous MLS log");
        try {
          await this.transition({ receive: { ...this.args(group), commit: bytes(entry.commit), proposals: entry.proposals.map(bytes) } });
        } catch (error) {
          if (!(error instanceof AgentOperationError)) throw error;
          // Expired Add packages can make an old log unreplayable. Discard the
          // failed agent, then validate a fresh GroupInfo and replace membership.
          await this.engine.reload();
          await this.join(group);
          continue groupLoop;
        }
        expected++;
        if (this.summary(group)?.record.status !== "active") break;
      }
      if (this.summary(group)?.record.status !== "active") continue;
      if (expected < log.head || log.next !== null) {
        if (expected === since) throw new Error("MLS log made no progress");
        // Catch-up pages do not consume the contention retry budget.
        attempt--; continue;
      }
      if (expected !== log.head) throw new Error("MLS log head mismatch");
      if (pending || log.pending.length) await this.engine.op({ pending: { ...this.args(group), pending: true } });
      for (const proposal of log.pending) await this.engine.op({ proposal: { ...this.args(group), bytes: bytes(proposal) } });
      await this.engine.op({ pending: { ...this.args(group), pending: false } });
      pending = false;
      const gate = this.gate(group);
      if (gate.rejoin) { await this.join(group); continue; }
      if (gate.server_pending || gate.blocked.length) {
        await this.engine.op({ commit: { ...this.args(group), adds: [], removes: [], now: this.now(), revocations: this.options.revocations() } });
        continue;
      }
      if (await this.admitMissing(group)) continue;
      if (version !== (this.groupVersions.get(group) ?? 0)) continue;
      this.syncedVersions.set(group, version);
      return;
    }
    throw new Error("MLS group is still securing; retrying");
  }
  private async devices(conversation: string): Promise<MlsDevice[]> {
    const all: MlsDevice[] = []; let after: string | null = null;
    do { const page: ResponseMap["mls.devices"] = await this.request("mls.devices", { uids: participants(conversation), after }); all.push(...page.devices); after = page.next; } while (after);
    return all;
  }
  private async admitMissing(group: string): Promise<boolean> {
    const local = this.summary(group)!;
    const members = new Set(local.members.map(m => m.device_key));
    const devices = await this.devices(local.conversation);
    if (!devices.some(d => !members.has(d.device_key))) return false;
    let after: string | null = null;
    const adds: number[][] = [];
    do {
      const page: ResponseMap["mls.key_packages.claim"] = await this.request("mls.key_packages.claim", { uids: participants(local.conversation), after });
      for (const p of page.packages) if (!members.has(p.device_key)) adds.push(bytes(p.package));
      after = page.next;
    } while (after);
    if (!adds.length) return false; // A device with no package joins itself on connect.
    await this.engine.op({ commit: { ...this.args(group), adds, removes: [], now: this.now(), revocations: this.options.revocations() } });
    return true;
  }
  private async queueHistory(group: string, recipient: string): Promise<void> {
    const local = this.summary(group);
    if (!local || !local.members.some(m => m.device_key === recipient) || recipient === this.options.identity.publicKey) return;
    const job: HistoryJob = { kind: "history", server: this.server, group, recipient, chunks: null };
    if (!this.engine.jobs.has(jobKey(job))) await this.engine.saveJobs([jobChange(job)]);
  }
  private assertGroupSynced(group: string): void {
    if (this.syncedVersions.get(group) !== (this.groupVersions.get(group) ?? 0)) throw new Error("MLS group changed; synchronize before sending");
  }
  private async sendHistory(): Promise<void> {
    jobs: for (const value of [...this.engine.jobs.values()]) {
      if (value.kind !== "history" || value.server !== this.server) continue;
      let job = value;
      if (this.syncedVersions.get(job.group) !== (this.groupVersions.get(job.group) ?? 0)) continue;
      const local = this.summary(job.group);
      if (!local?.members.some(m => m.device_key === job.recipient)) {
        await this.engine.saveJobs([jobChange(job, true)]); continue;
      }
      const gate = this.gate(job.group);
      if (gate.status !== "active" || gate.rejoin || gate.server_pending || gate.blocked.length || gate.pending_commit) continue;
      if (job.chunks === null) {
        const recipient = (await this.devices(local.conversation)).find(d => d.device_key === job.recipient);
        if (!recipient) continue;
        await this.engine.op<number[][]>({ share_history: { ...this.args(job.group), recipient: { device_key: recipient.device_key, hpke_key: recipient.hpke_key, created_at: recipient.created_at, signature: recipient.signature }, now: this.now(), revocations: this.options.revocations() } }, chunks => {
          job = { ...job, chunks: chunks.map(encoded) }; return [jobChange(job)];
        });
      }
      for (const chunk of job.chunks!) {
        const current = this.gate(job.group);
        if (current.status !== "active" || current.rejoin || current.server_pending || current.blocked.length || current.pending_commit) continue jobs;
        try { await this.request("mls.history.share", { group: job.group, recipient: job.recipient, chunk }, () => {
          this.assertGroupSynced(job.group);
          const gate = this.gate(job.group);
          if (gate.status !== "active" || gate.rejoin || gate.server_pending || gate.blocked.length || gate.pending_commit) throw new Error("history sharing blocked");
        }); }
        catch (error) {
          if (!(error instanceof RequestError)) throw error;
          // A quota/rejection for one recipient must not starve other jobs.
          // Retain the exact bytes; only a success permits deleting this job.
          await this.engine.reload(); this.options.onError?.(error); continue jobs;
        }
      }
      await this.engine.saveJobs([jobChange(job, true)]);
    }
  }
  private async importHistory(): Promise<void> {
    let after: string | null = null;
    do {
      const page: ResponseMap["mls.history.fetch"] = await this.request("mls.history.fetch", { group: null, after });
      for (const chunk of page.chunks) {
        if (this.summary(chunk.group)?.record.status !== "active") continue;
        try { await this.engine.op({ import_history: { ...this.args(chunk.group), bytes: bytes(chunk.chunk) } }); }
        catch (error) {
          if (!(error instanceof AgentOperationError)) throw error;
          // An invalid or no-longer-authorized sender must not prevent importing
          // other chunks. Never acknowledge an import that did not succeed.
          await this.engine.reload(); this.options.onError?.(error); continue;
        }
        await this.request("mls.history.ack", { ids: [chunk.id] });
      }
      after = page.next;
    } while (after);
  }
  private async requestMissing(): Promise<void> {
    for (const [group, epochs] of this.missing) {
      for (const epoch of epochs) {
        const record = this.engine.query<null | string | { derived?: unknown; imported?: unknown }>({ epoch: { ...this.args(group), epoch } });
        if (record && record !== "conflicted") epochs.delete(epoch);
      }
      if (!epochs.size) { this.missing.delete(group); continue; }
      if (this.now() - (this.lastRequest.get(group) ?? -Infinity) < 60_000) continue;
      if (this.summary(group)?.record.status !== "active") continue;
      await this.request("mls.history.request", { group }); this.lastRequest.set(group, this.now());
    }
  }
  async openConversation(conversation: string): Promise<void> {
    participants(conversation);
    return this.engine.run(async () => {
      this.activeGeneration = this.generation; this.assertCurrent(this.activeGeneration);
      this.statuses.set(conversation, "creating"); this.notify();
      const known = (await this.groups()).find(g => g.conversation === conversation);
      for (const g of this.inventory()) if (g.conversation === conversation && g.record.status === "creating") {
        await this.transition({ settle: { ...this.args(encoded(g.group)), winner: known ? bytes(known.group) : null } });
      }
      if (known) { await this.syncGroup(known.group, known.pending); this.updateStatuses(); return; }
      const d = await descriptor(this.options.identity, this.uid, this.server, conversation, this.now());
      const info = await this.engine.op<number[]>({ create: { descriptor: d.bytes } });
      let winner: string;
      try { winner = (await this.request("mls.group.create", { conversation, group_info: encoded(info) })).group; }
      catch (error) {
        if (!(error instanceof RequestError) || error.code !== "conflict") throw error;
        // Conflict text is not a typed protocol field. Resolve through groups.
        const found = (await this.groups()).find(g => g.conversation === conversation);
        if (!found) throw error;
        winner = found.group;
      }
      await this.transition({ settle: { ...this.args(d.group), winner: bytes(winner) } });
      await this.syncGroup(winner);
      await this.sendHistory();
      this.updateStatuses();
    }).catch(error => { this.markSecuring(); this.schedule(1000); throw error; });
  }
  /** Signing runs inside the queue, with the exact current group/epoch. The
   * builder may await WebCrypto; close waits for it and its write to finish. */
  async sealMessage(conversation: string, build: (context: SealContext) => Promise<Plaintext>): Promise<{ group: string; epoch: number; ciphertext: string }> {
    return this.engine.run(async () => {
      const generation = this.generation; this.activeGeneration = generation; this.assertCurrent(generation);
      const local = this.inventory().find(g => g.conversation === conversation);
      if (!local) throw new Error("open the conversation first");
      const group = encoded(local.group);
      await this.syncGroup(group);
      const [epoch] = this.engine.query<[string, number[]]>({ sending_key: { ...this.args(group), now: this.now(), revocations: this.options.revocations() } });
      const plaintext = await build({ server_key: this.server, conversation, group_id: group, epoch });
      this.assertCurrent(generation); this.assertGroupSynced(group);
      const ciphertext = await this.engine.op<number[]>({ seal_message: { context: { server_key: this.server, conversation, group_id: bytes(group), epoch, message_id: [...decodeFixed(plaintext.signature.message_id, 16)], revision: plaintext.signature.revision }, plaintext, now: this.now(), revocations: this.options.revocations() } });
      this.assertCurrent(generation); this.assertGroupSynced(group);
      return { group, epoch: wireEpoch(epoch), ciphertext: encoded(ciphertext) };
    });
  }
  /** The caller MUST verify the returned v3 signature before displaying it. */
  async openMessage(message: SealedFields): Promise<OpenResult> {
    return this.engine.run(async () => {
      let result: OpenResult;
      try {
        const epoch = String(wireEpoch(message.epoch));
        result = await this.engine.op<OpenResult>({ open_message: { context: { server_key: this.server, conversation: message.conversation, group_id: [...decodeFixed(message.group, 32)], epoch, message_id: [...decodeFixed(message.message_id, 16)], revision: message.revision }, ciphertext: bytes(message.ciphertext) } });
        if ("reason" in result && result.reason === "no_key") {
          const epochs = this.missing.get(message.group) ?? new Set<string>(); epochs.add(epoch); this.missing.set(message.group, epochs);
          this.schedule();
        }
      } catch { throw new Error("invalid sealed message envelope"); }
      return result;
    }).catch(() => ({ reason: "cannot_open" }));
  }
  close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    this.stopped = true; this.generation++;
    clearTimeout(this.retry);
    for (const unsubscribe of this.unsubscribers) unsubscribe();
    this.options.link.close();
    this.closePromise = (async () => {
      try { await this.engine.close(); } finally { await this.release(); }
      this.markSecuring();
    })();
    return this.closePromise;
  }
}
