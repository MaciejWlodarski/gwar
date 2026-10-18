import type { Link } from "../net/connection";
import { httpOriginFromWsUrl } from "../net/address";
import { RequestError } from "../net/connection";
import type { Identity } from "../net/identity";
import { uidForPublicKey } from "../net/identity";
import { decodeFixed } from "../net/handshake";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { Op, RequestInput, ResponseMap } from "../net/protocol";
import type { MlsGroup } from "../proto/MlsGroup";
import type { MlsDevice } from "../proto/MlsDevice";
import type { MlsIncarnation } from "../proto/MlsIncarnation";
import { authenticatePlaintext } from "./plaintext";
import { channelOf, credential, descriptor, participants, type Successor } from "./descriptor";
import { AgentOperationError, Engine, acquireDeviceLock, jobChange, jobKey } from "./engine";
import { openStorage, type Persistence } from "./storage";
import { loadAgent, loadVoiceAgent } from "./wasm";
import { VoiceCall } from "./voice";
import type { VoiceState } from "../proto/VoiceState";
import type { Managers } from "../proto/Managers";
import type { OwnerInfo } from "../proto/OwnerInfo";
import type { AccessView, AgentFactory, Change, OwnerCheck, OwnerGeneration, OwnerView, CommitOutbox, ConversationStatus, Gate, GroupSummary, HistoryJob, OpenResult, PackageJob, Plaintext, RequestJob, Resolution, Revocation, Roster, SealedFields } from "./types";
import { repairable, usable } from "./types";
import { memberListRenewalDue } from "../lib/managers";
const bytes = (value: string): number[] => [...decodeBase64Url(value)];
/** The agent's error when a private group's lists give this device no current authority. */
const WAITING = "waiting for a channel manager";
/** The agent's refusal of a plain restart chain for a private conversation. */
const PRIVATE_CHAIN = "needs its final GroupInfo";
const encoded = (value: number[]): string => encodeBase64Url(new Uint8Array(value));
/** The DS protocol currently uses safe JSON integers; core epochs remain u64 strings. */
function wireEpoch(value: string | number): number {
  const n = Number(value);
  if (!Number.isSafeInteger(n) || n < 0 || String(n) !== String(value)) throw new Error("epoch outside delivery protocol range");
  return n;
}
export interface SealContext { server_key: string; conversation: string; group_id: string; epoch: string }
const DAY = 86400_000;
/** A device updates its own leaf in a public group after this long without an own commit there. */
const LEAF_UPDATE_AFTER = 7 * DAY;
/** Two checks finding the server behind a live pin, this far apart, restart the conversation. */
const RESTART_AFTER = 60_000;
interface Backoff { at: number; delay: number }
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
  /** Another tab of this device asked for the session while this runtime holds
   * the lock; the caller closes it. Without it, a second tab waits for the lock. */
  onTakeover?: () => void;
  /** Fetches public groups' ratchet trees over HTTP; the global fetch by default. */
  fetch?: typeof fetch;
  /** The owner an invite link named (`owner={uid}`), checked on first contact. */
  inviteOwner?: string;
  /** The server's owner record is no extension of this device's pin: the caller disconnects. */
  onOwnerChanged?: () => void;
  /** An owner reset: a neutral notice the user may accept (docs/e2e-build.md, section 6, 6b). */
  onOwnerGeneration?: (notice: OwnerGeneration) => void;
}
/** A ratchet tree larger than this is refused (about 15 000 devices). */
const MAX_TREE_BYTES = 8 * 1024 * 1024;
/** A tree download, headers and body, ends after this. */
const TREE_TIMEOUT_MS = 30_000;
/** A newer tab of this device asked for the session while this one waited for it. */
export class DeviceTakenOver extends Error {
  constructor() { super("This device's session moved to another tab"); }
}
class GroupChanged extends Error {
  constructor() { super("MLS group changed; synchronize before sending"); }
}
class HeldWelcome extends Error {
  constructor() { super("The Welcome's conversation is held for a restart"); }
}
/** This device just restarted the conversation: the old group's sync stops, without failing. */
class Restarted extends Error {
  constructor() { super("The conversation was restarted"); }
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
  private readonly rosters = new Map<string, Roster>();
  private readonly listeners = new Set<() => void>();
  private readonly missing = new Map<string, Set<string>>();
  private readonly lastRequest = new Map<string, number>();
  /** Encrypted channels by id with their `history`, from the latest Welcome and channel events. */
  private channels: Map<number, boolean> | undefined;
  /** Encrypted private channels (6c): their groups carry signed member lists. */
  private privateChannels = new Set<number>();
  /** Members chosen in the interface for a private channel's first group, by conversation. */
  private readonly pendingMembers = new Map<string, string[]>();
  /** Conversations whose lists give this device no current authority yet (6c, "Newcomers"). */
  private readonly waiting = new Set<string>();
  private serverChat = false;
  /** The owner and managers this device verified, and the server's latest managers' view. */
  private owner: OwnerView | null = null;
  private managers: Managers | null = null;
  /** Whether the server's manager pair is current for this device (false: it needs renewing). */
  private managersCurrent = true;
  private managersRenewable = false;
  /** The server list `managersCurrent` and `ownerView()` were last checked against. */
  private verifiedList: string | null = null;
  private ownerChecks: Promise<void> = Promise.resolve();
  private ownerRetries = 0;
  /** This session's file access token: the bearer for the tree endpoint. */
  private fileAccess: string | null = null;
  /** The conversation of every group the server listed, for groups not joined yet. */
  private readonly conversations = new Map<string, string>();
  /** Tree downloads in flight, ended when the connection drops or the runtime closes. */
  private readonly downloads = new Set<AbortController>();
  /** When a public group may next be created or joined, per conversation. */
  private readonly groupDue = new Map<string, Backoff>();
  /** When a history job whose preparation failed may be tried again, per job key. */
  private readonly historyDue = new Map<string, { at: number; delay: number }>();
  private nextJoin = 0;
  /** Conversations not followed in this pass: the server holds no group this
   * device accepts (it lost a restart, or the chain is not accepted yet). */
  private held = new Set<string>();
  /** Pending restart triggers by conversation: the live pin and checkpoint
   * first found behind, and when (docs/e2e-build.md "Restarts"). */
  private readonly triggers = new Map<string, { key: string; at: number }>();
  /** Private groups (6c): offered lists to commit, by group; when a due repair may be committed;
   * offers this device sent for a GroupInfo it could not join (one each). */
  private readonly offers = new Map<string, string>();
  private readonly repairAt = new Map<string, number>();
  private readonly offered = new Map<string, number>();
  private readonly renewedAt = new Map<string, number>();
  private wakeAt: number | undefined;
  /** Volatile voice calls under this device lease, closed with it. */
  private readonly calls = new Set<VoiceCall>();
  private readonly now: () => number;
  private constructor(private readonly options: RuntimeOptions, private readonly engine: Engine, private readonly release: () => Promise<void>) { this.now = options.now ?? Date.now; }

  static async connect(options: RuntimeOptions): Promise<MlsRuntime> {
    if (options.link.status.state !== "idle") throw new Error("MLS must acquire its lock before connecting");
    const locks = options.locks ?? globalThis.navigator?.locks;
    if (!locks) throw new Error("Web Locks are required for MLS");
    // The newest tab wins, like the server's one session per device.
    const takeover = options.onTakeover && typeof BroadcastChannel !== "undefined" ? new BroadcastChannel(`gwar-mls:${options.identity.publicKey}`) : undefined;
    const waiting = new AbortController();
    const abortWaiting = () => waiting.abort(options.signal!.reason);
    options.signal?.addEventListener("abort", abortWaiting, { once: true });
    if (options.signal?.aborted) abortWaiting();
    // Requests are ordered by time, then a random tie-break: a tab, waiting or
    // holding, yields only to a newer one, so contenders cannot evict each other.
    const mine = [performance.timeOrigin + performance.now(), Math.random()] as const;
    const newer = (other: unknown) => {
      if (!Array.isArray(other) || other.length !== 2 || !other.every(n => typeof n === "number")) return false;
      const [at, tie] = other as [number, number];
      return at > mine[0] || (at === mine[0] && tie > mine[1]);
    };
    let holding = false;
    if (takeover) {
      takeover.onmessage = event => {
        if (!newer(event.data)) return; // Older requests have already yielded to this one.
        if (holding) options.onTakeover!();
        else waiting.abort(new DeviceTakenOver());
      };
      takeover.postMessage(mine);
    }
    let release: () => Promise<void>;
    try { release = await acquireDeviceLock(options.identity.publicKey, locks, waiting.signal); }
    catch (error) { takeover?.close(); throw waiting.signal.reason instanceof DeviceTakenOver ? waiting.signal.reason : error; }
    finally { options.signal?.removeEventListener("abort", abortWaiting); }
    holding = true;
    let runtime: MlsRuntime | undefined;
    let storage: Persistence | undefined;
    try {
      storage = await (options.storage?.() ?? openStorage(options.identity.publicKey));
      const factory = options.factory ?? await loadAgent(options.seed, credential(options.identity), options.now ?? Date.now);
      runtime = new MlsRuntime(options, new Engine(factory, storage), release);
      if (takeover) runtime.unsubscribers.push(() => takeover.close());
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
          for (const download of runtime!.downloads) download.abort();
          runtime!.generation++;
          runtime!.lastPublish = -Infinity;
          runtime!.syncedVersions.clear();
          runtime!.markSecuring();
          // Connection loss closes voice and forgets its secrets; no counter resumes.
          for (const call of runtime!.calls) call.close();
          runtime!.calls.clear();
          if (status.state === "closed") void runtime!.close();
        }
      }));
      runtime.unsubscribers.push(options.link.onWelcome(welcome => {
        const server = options.link.serverKey;
        if (!server || (runtime!.server && runtime!.server !== server)) {
          options.onError?.(new Error("MLS pinned server changed"));
          void runtime!.close(); return;
        }
        runtime!.server = server;
        runtime!.fileAccess = welcome.file_access;
        runtime!.serverChat = welcome.server.server_chat.encrypted;
        if (!runtime!.serverChat) { runtime!.statuses.delete("server"); runtime!.rosters.delete("server"); }
        runtime!.channels = new Map(welcome.channels.filter(c => c.encrypted).map(c => [c.id, c.history]));
        runtime!.privateChannels = new Set(welcome.channels.filter(c => c.encrypted && c.private).map(c => c.id));
        runtime!.managers = welcome.managers ?? null;
        runtime!.checkOwner(welcome.owner ?? null);
        runtime!.schedule();
      }));
      runtime.unsubscribers.push(options.link.onEvent(event => {
        for (const call of runtime!.calls) call.event(event);
        if (event.ev === "channel.created" && event.d.encrypted) {
          runtime!.channels?.set(event.d.id, event.d.history);
          if (event.d.private) runtime!.privateChannels.add(event.d.id);
          runtime!.schedule();
        }
        // A hidden channel's group is forgotten like a deleted one's; its tombstone keeps the
        // checkpoints for when it is shown again (6c).
        if ((event.ev === "channel.deleted" || event.ev === "channel.hidden") && runtime!.channels?.delete(event.d.channel)) {
          runtime!.privateChannels.delete(event.d.channel);
          runtime!.schedule();
        }
        if (event.ev === "owner.updated") runtime!.checkOwner(event.d);
        if (event.ev === "managers.updated") { runtime!.managers = event.d; runtime!.checkOwner(undefined); }
        // A member commits offered lists (it validates them first); a waiting newcomer retries.
        if (event.ev === "mls.access.offered") { runtime!.offers.set(event.d.group, event.d.extension); runtime!.schedule(); return; }
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
      else { takeover?.close(); storage?.close(); await release(); }
      throw error;
    }
  }
  status(conversation: string): ConversationStatus {
    const status = this.statuses.get(conversation) ?? "securing";
    return status !== "ready" && this.waiting.has(conversation) ? "waiting" : status;
  }
  /** The server's owner and the managers of private channels as this device verified them. */
  ownerView(): OwnerView | null { return this.owner; }
  /** False when the server shows a manager pair that is no longer current (an expired, revoked or
   * former signer): the owner or a confirmer renews it. */
  serverManagersCurrent(): boolean { return this.managersCurrent; }
  /** The server's pair is not current and this device may renew it (checked by the agent). */
  serverManagersRenewable(): boolean { return this.managersRenewable; }
  /** True once the server's newest list has been checked: until then the view describes an older one. */
  managersSettled(): boolean { return !this.managers?.list || this.verifiedList === this.managers.list; }
  /**
   * Checks the server's owner chain against this device's pin, then its newest managers
   * (docs/e2e-build.md, section 6, 6b). `owner` undefined re-checks only the managers.
   * Checks run one after another.
   */
  /** A managers-only check from `sync` is queued (coalesces bursts of events). */
  private managersRecheck = false;
  private checkOwner(owner: OwnerInfo | null | undefined): void {
    const generation = this.generation;
    this.ownerChecks = this.ownerChecks
      .then(() => owner === undefined ? this.acceptManagers() : this.followOwner(owner, generation))
      .catch(error => { if (!this.stopped) this.options.onError?.(error); });
  }
  private async followOwner(owner: OwnerInfo | null, generation: number): Promise<void> {
    const server = this.server;
    if (!owner) {
      // A server that dropped the owner this device pinned, or that an invite said has one, is
      // not the server the device knows.
      const pinned = await this.engine.run(async () => this.engine.withAgent(a => a.owner_view?.(server) ?? null));
      if (pinned || this.options.inviteOwner) this.options.onOwnerChanged?.();
      return;
    }
    let after = 0, genesis = "";
    const statements: string[] = [], signatures: string[] = [];
    try {
      for (;;) {
        const page = await this.request("owner.chain", { generation: owner.generation, after }, undefined, generation);
        genesis = page.genesis;
        for (const link of page.links) { statements.push(link.statement); signatures.push(link.signature); }
        if (page.next === null || page.links.length === 0) break;
        after = page.next;
      }
    } catch (error) {
      // A transient failure (not a verdict on the chain): check again a few times, then report.
      if (this.ownerRetries++ < 5 && !this.stopped) setTimeout(() => { if (this.generation === generation) this.checkOwner(owner); }, 2000 * this.ownerRetries);
      throw error;
    }
    this.ownerRetries = 0;
    this.assertCurrent(generation);
    const check = await this.engine.run(() => this.engine.call(a => a.check_owner!(server, genesis, statements, signatures, this.options.inviteOwner))) as OwnerCheck;
    if (check.kind === "changed") { this.options.onOwnerChanged?.(); return; }
    if (check.kind === "new_generation") {
      this.options.onOwnerGeneration?.({
        generation: check.generation, owner: check.owner,
        accept: async () => {
          await this.engine.run(() => this.engine.call(a => a.accept_generation!(server, genesis, statements, signatures)));
          this.checkOwner(undefined);
        },
      });
      return;
    }
    await this.acceptManagers();
  }
  /** The server's current anchor envelope, if it has one. */
  private serverAnchor(): Uint8Array | undefined {
    return this.managers?.anchor ? decodeBase64Url(this.managers.anchor) : undefined;
  }
  /** The next confirmer anchor's statement, for the owner's account key to sign. */
  nextAnchor(confirmers: string[]): Promise<string> {
    const server = this.server, shown = this.serverAnchor();
    return this.engine.run(async () => this.engine.withAgent(a => a.next_anchor!(server, confirmers, this.now(), shown)));
  }
  /** The statement moving ownership to `newOwner`, for the owner's account key to sign. */
  transferStatement(accountKey: string, newOwner: string): Promise<string> {
    const server = this.server, anchor = this.serverAnchor();
    return this.engine.run(async () => this.engine.withAgent(a => a.transfer_statement!(server, accountKey, newOwner, anchor, this.now())));
  }
  async transferOwnership(statement: string, signature: string): Promise<void> {
    await this.request("owner.transfer", { statement, signature }, undefined, this.generation);
  }
  /**
   * Signs the next manager list with this device and installs it with `anchor` (a newly signed
   * anchor envelope) or the server's current anchor. `managers` null renews: the agent keeps the
   * server's list when it verifies, else the accepted one, and refuses when there is neither.
   */
  async publishManagers(managers: string[] | null, anchor?: Uint8Array): Promise<void> {
    const server = this.server, shown = this.serverAnchor(), under = anchor ?? shown;
    if (!under) throw new Error("no confirmer anchor yet");
    // A change is made only against the server's newest pair, checked and current: otherwise the
    // view this change was derived from may be older than the list it would replace. A plain
    // renewal keeps the server's verified membership and is what makes a pair current again.
    if ((managers !== null || anchor) && this.managers?.list && !(this.managersSettled() && this.managersCurrent)) {
      throw new Error("the manager list is changing; try again");
    }
    const floor = this.managers?.list ? decodeBase64Url(this.managers.list) : undefined;
    const list = await this.engine.run(async () => this.engine.withAgent(a => a.sign_manager_list!(a.next_list!(server, under, managers ?? undefined, this.now(), shown, floor, this.options.revocations()))));
    await this.request("managers.update", { anchor: encodeBase64Url(under), list: encodeBase64Url(list) }, undefined, this.generation);
  }
  private async acceptManagers(): Promise<void> {
    const server = this.server, managers = this.managers;
    if (!server) return;
    if (managers?.anchor && managers.list) {
      const anchor = decodeBase64Url(managers.anchor), list = decodeBase64Url(managers.list);
      const check = await this.engine.run(() => this.engine.call(a => a.accept_managers?.(server, anchor, list, this.now(), this.options.revocations())));
      this.managersCurrent = check?.kind !== "not_current" && check?.kind !== "renewable";
      this.managersRenewable = check?.kind === "renewable";
      // A newly accepted pair may make private groups' lists recordable: a pass observes them.
      if (check?.kind === "accepted") this.schedule();
    }
    this.owner = await this.engine.run(async () => this.engine.withAgent(a => a.owner_view?.(server) ?? null));
    this.verifiedList = managers?.list ?? null;
    this.notify();
  }
  /** Who can read an active conversation (devices by uid) and whether new members get earlier messages. */
  roster(conversation: string): Roster | null { return this.rosters.get(conversation) ?? null; }
  subscribe(listener: () => void): () => void { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  private notify(): void { for (const listener of this.listeners) listener(); }
  private markSecuring(): void { for (const key of this.statuses.keys()) this.statuses.set(key, "securing"); this.notify(); }
  private assertCurrent(generation: number): void {
    if (this.stopped || generation !== this.generation || !this.options.link.isOnline || this.options.link.serverKey !== this.server) throw new Error("MLS connection changed");
  }
  /** One shared budget for chat and voice: acquisitions are serialized, so
   * concurrent waiters cannot each reset an empty bucket. */
  private token(): Promise<void> {
    const next = this.tokens.then(async () => {
      const now = performance.now();
      this.requestTokens = Math.min(12, this.requestTokens + (now - this.refilled) / 100);
      this.refilled = now;
      if (this.requestTokens < 1) {
        await new Promise(resolve => setTimeout(resolve, (1 - this.requestTokens) * 100));
        this.refilled = performance.now(); this.requestTokens = 1;
      }
      this.requestTokens--;
    });
    this.tokens = next.catch(() => {});
    return next;
  }
  private tokens: Promise<void> = Promise.resolve();
  private async request<O extends Op>(op: O, data: RequestInput<O>, guard?: () => void, generation = this.activeGeneration): Promise<ResponseMap[O]> {
    this.assertCurrent(generation);
    // Leave room for chat/voice/control traffic in the connection's 20/s bucket.
    await this.token();
    this.assertCurrent(generation);
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
    // Revocation refreshes and membership events reach voice gates at once.
    for (const call of this.calls) call.recheck();
    // ... and the manager pair: a revoked or expired signer makes it no longer current (6b).
    if (this.managers?.anchor && !this.managersRecheck) {
      this.managersRecheck = true;
      this.checkOwner(undefined);
      void this.ownerChecks.then(() => { this.managersRecheck = false; });
    }
    if (this.syncing) { this.dirty = true; return this.syncing; }
    let failed = false;
    this.syncing = this.engine.run(async () => {
      // Bound each pass; events arriving during it request one subsequent pass.
      this.dirty = false; this.wakeAt = undefined;
      this.activeGeneration = this.generation;
      this.assertCurrent(this.generation);
      try { await this.observePrivateGroups(); await this.synchronize(); }
      catch (error) {
        // Rosters follow the durable state the failed pass left (a restart
        // accepted before a join failed), before any later operation runs.
        await this.engine.reload().then(() => this.updateStatuses(true), () => {});
        throw error;
      }
      this.updateStatuses();
    }).catch(error => { failed = true; this.markSecuring(); throw error; }).finally(() => {
      this.syncing = undefined;
      // Periodic refill, revocation/expiry checks and missing history retries;
      // sooner for a channel group whose create or join comes due.
      const wake = this.wakeAt === undefined ? 15_000 : Math.max(200, Math.min(15_000, this.wakeAt - this.now()));
      this.schedule(!failed && this.dirty ? 200 : wake);
    });
    return this.syncing;
  }
  /** A plaintext Welcome leaves persisted server groups and jobs dormant. */
  private enabled(conversation: string | undefined): boolean { return conversation !== "server" || this.serverChat; }
  private inventory(): GroupSummary[] { return this.engine.query({ inventory: { server: this.server } }); }
  private summary(group: string): GroupSummary | undefined { return this.inventory().find(g => encoded(g.group) === group); }
  private args(group: string) { return { server: this.server, group: [...decodeFixed(group, 32)] }; }
  /** A private channel's creation settles as of now (staged adoption, 6c). */
  private settleArgs(group: string) {
    return this.summary(group)?.private ? { ...this.args(group), now: this.now(), revocations: this.options.revocations() } : this.args(group);
  }
  private gate(group: string): Gate { return this.engine.query({ gate: { ...this.args(group), now: this.now(), revocations: this.options.revocations() } }); }
  private outbox(group: string): CommitOutbox | null { return this.engine.query({ outbox: this.args(group) }); }
  private updateStatuses(securing = false): void {
    this.rosters.clear();
    // A conversation is its live pin; a first creation stands for it until settled.
    const shown = new Map<string, GroupSummary>();
    for (const g of this.inventory()) {
      if (!this.enabled(g.conversation)) continue;
      if (g.live || (g.record.status === "creating" && !shown.get(g.conversation)?.live)) shown.set(g.conversation, g);
    }
    for (const [conversation, g] of shown) {
      const gate = this.gate(encoded(g.group));
      const ready = usable(gate) && !this.held.has(conversation);
      this.statuses.set(conversation, gate.status === "creating" ? "creating" : ready && !securing ? "ready" : "securing");
      const first = g.record.evidence?.[this.uid]?.[0]?.first;
      // A live pin still to be joined (a restart just accepted) already names the conversation's group.
      if (g.live) this.rosters.set(conversation, { group: encoded(g.group), restarted: g.incarnation !== "0", history: g.history, members: g.members, since: first === undefined ? null : Number(first) });
    }
    this.notify();
  }
  private livePin(conversation: string): GroupSummary | undefined { return this.inventory().find(g => g.conversation === conversation && g.live); }
  private historyIntents(before: GroupSummary[]): Change[] {
    const changes: Change[] = [];
    for (const g of this.inventory()) {
      // Public groups share history by the server's assignments instead.
      if (!g.history || g.record.status !== "active" || !g.conversation.startsWith("dm:")) continue;
      const old = before.find(b => encoded(b.group) === encoded(g.group) && b.record.status === "active");
      for (const member of g.members) {
        if (member.device_key === this.options.identity.publicKey || old?.members.some(m => m.device_key === member.device_key)) continue;
        const job: HistoryJob = { kind: "history", server: this.server, group: encoded(g.group), recipient: member.device_key, publication: null, chunks: null };
        if (!this.engine.jobs.has(jobKey(job))) changes.push(jobChange(job));
      }
    }
    return changes;
  }
  /** Runtime jobs of superseded groups go with them, in the same write. */
  private supersededJobs(): Change[] {
    const old = new Set(this.inventory().filter(g => g.record.status === "superseded").map(g => encoded(g.group)));
    return [...this.engine.jobs.values()].filter(job => job.kind !== "packages" && job.server === this.server && old.has(job.group)).map(job => jobChange(job, true));
  }
  private async transition<T>(request: unknown, validate?: (result: T) => void): Promise<T> {
    const before = this.inventory();
    return this.engine.op<T>(request, result => { validate?.(result); return [...this.historyIntents(before), ...this.supersededJobs()]; });
  }
  /** A private group's repair (6c): commits offered lists it validates, or, when its own
   * authority says so, the accepted pair it carries or the removals the lists require, after a
   * random delay of up to 10 s so that concurrent repairers settle on the first commit. Returns
   * whether it committed something (the caller then submits it). */
  private async repairAccess(group: string, gate: Gate): Promise<boolean> {
    // An offer waits while a newer manager pair is still being checked: it may carry that pair.
    // It is kept until it brings nothing more (committed, or older than what the group holds), so
    // an expired or losing commit of it is built again.
    const offer = this.offers.get(group);
    if (offer !== undefined && this.managersSettled()) {
      try {
        await this.engine.op({ adopt_offer: { ...this.args(group), extension: bytes(offer), now: this.now(), revocations: this.options.revocations() } });
        return true;
      } catch (error) {
        if (!(error instanceof AgentOperationError)) throw error;
        await this.engine.reload(); // Nothing (more) to commit for this device: dropped.
        if (this.offers.get(group) === offer) this.offers.delete(group);
      }
    }
    if (!repairable(gate)) { this.repairAt.delete(group); return this.renewMembers(group, gate); }
    const at = this.repairAt.get(group);
    if (at === undefined) { this.repairAt.set(group, this.now() + Math.random() * 10_000); }
    if (at === undefined || at > this.now()) { this.wakeAt = Math.min(this.wakeAt ?? Infinity, this.repairAt.get(group)!); return false; }
    this.repairAt.delete(group);
    await this.engine.op({ commit_access: { ...this.args(group), members: null, now: this.now(), revocations: this.options.revocations() } });
    return true;
  }
  /** The owner's, confirmers' and managers' clients re-sign a member list (seq + 1, the same
   * members) when it is older than 60 days, or its signer's certificate expires within 7 days or
   * was revoked (6c, "Newcomers"); at most every ten minutes per group, and not for expiry by a
   * device whose own certificate expires as soon. */
  private async renewMembers(group: string, gate: Gate): Promise<boolean> {
    const list = this.renewalDue(group, gate);
    if (!list) return false;
    const now = this.now();
    this.renewedAt.set(group, now);
    await this.engine.op({ commit_access: { ...this.args(group), members: list.members, now, revocations: this.options.revocations() } });
    return true;
  }
  /** The member list to re-sign now, if this device's user is an authority and it is due. */
  private renewalDue(group: string, gate: Gate): AccessView | null {
    const me = this.uid, view = this.owner;
    if (gate.access?.kind !== "current" || !view || !(view.owner === me || view.confirmers.includes(me) || view.managers.includes(me))) return null;
    if (this.now() - (this.renewedAt.get(group) ?? -Infinity) < 10 * 60_000) return null;
    const list = this.engine.query<AccessView | null>({ access_view: this.args(group) });
    if (!list || !memberListRenewalDue(list, this.now(), this.options.identity.device?.expires_at ?? Infinity, this.options.revocations())) return null;
    return list;
  }
  /** Offers fresh lists for a private group this device could not join, when its user is the
   * owner, a confirmer or a manager. */
  private async offerAccess(group: string, info: number[], tree: number[] | null): Promise<void> {
    // Once per GroupInfo and accepted pair: a newer pair needs a fresh offer.
    const me = this.uid, view = this.owner;
    const key = `${group}:${view?.generation}:${view?.anchor_seq}:${view?.list_seq}:${encoded(info)}`;
    // Sent again every ten minutes while the join still waits: a member may have been offline.
    const sent = this.offered.get(key);
    if (!view || (sent !== undefined && this.now() - sent < 10 * 60_000) || !(view.owner === me || view.confirmers.includes(me) || view.managers.includes(me))) return;
    try {
      const extension = this.engine.query<number[]>({ offer_access: { server: this.server, group_info: info, tree, members: null, now: this.now(), revocations: this.options.revocations() } });
      await this.request("mls.access.offer", { group, extension: encoded(extension) });
      // A failed offer is retried with the join's backoff; a delivered one after ten minutes.
      this.offered.set(key, this.now());
    } catch (error) {
      this.options.onError?.(error);
    }
  }
  /** At the start of every pass, before any replay: records access an interrupted pass, or a
   * manager pair accepted since, left unobserved. */
  private async observePrivateGroups(): Promise<void> {
    // Also a removal's lists, until they are recorded (they may wait for a manager pair).
    for (const g of this.inventory()) if (g.private && (g.record.status === "active" || g.record.removal_access)) await this.observeAccess(encoded(g.group));
  }
  /** After every transition of a private channel's group, before the next one: records a member
   * list current on first sight as the newest accepted, so a later commit cannot roll it back. */
  private async observeAccess(group: string): Promise<void> {
    if (!this.summary(group)?.private) return;
    await this.engine.op({ observe_access: { ...this.args(group), now: this.now(), revocations: this.options.revocations() } });
  }
  private async groups(): Promise<MlsGroup[]> {
    const all: MlsGroup[] = []; let after: string | null = null;
    do { const page: ResponseMap["mls.groups"] = await this.request("mls.groups", { after }); all.push(...page.groups); after = page.next; } while (after);
    for (const g of all) this.conversations.set(g.group, g.conversation);
    return all.filter(g => g.conversation.startsWith("dm:") || channelOf(g.conversation) !== undefined || (g.conversation === "server" && this.serverChat));
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
        // Nobody writes to an earlier incarnation: its Welcome has nothing to give.
        const local = this.summary(welcome.group);
        if (!this.enabled(local?.conversation ?? this.conversations.get(welcome.group))) continue;
        if (local?.record.status === "superseded") {
          await this.request("mls.welcomes.ack", { ids: [welcome.id] });
          continue;
        }
        // A held conversation imports nothing; the Welcome waits on the server.
        if (local && this.held.has(local.conversation)) continue;
        // Replay stale membership first, stopping at eviction. A readmission
        // Welcome must be imported before an external rejoin can skip its epoch.
        const known = local?.record.welcome_ids?.includes(welcome.id);
        const caughtUp = known ? false : await this.syncGroup(welcome.group, false, true);
        if (caughtUp === true) {
          await this.request("mls.welcomes.ack", { ids: [welcome.id] });
          continue;
        }
        try {
          const input = { server: this.server, id: welcome.id, bytes: bytes(welcome.welcome) };
          const request = caughtUp === "pruned" ? { replace_stale_welcome: { ...input, group: bytes(welcome.group) } } : { welcome: input };
          await this.transition<{ group: number[]; duplicate: boolean }>(request, result => {
            if (encoded(result.group) !== welcome.group) throw new Error("Welcome group mismatch");
            // A first Welcome into a held conversation is not persisted either.
            if (this.held.has(this.summary(welcome.group)?.conversation ?? "")) throw new HeldWelcome();
          });
        } catch (error) {
          if (error instanceof HeldWelcome) { await this.engine.reload(); continue; }
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
    for (const g of this.inventory()) if (g.record.status === "creating" && this.enabled(g.conversation)) {
      const winner = groups.find(remote => remote.conversation === g.conversation);
      await this.transition({ settle: { ...this.settleArgs(encoded(g.group)), winner: winner ? bytes(winner.group) : null } });
    }
    this.held = await this.acceptRestarts(groups);
    await this.welcomes();
    for (const g of groups) if (g.conversation.startsWith("dm:") && !this.held.has(g.conversation)) {
      try { await this.syncGroup(g.group, g.pending); }
      catch (error) { if (!(error instanceof Restarted)) throw error; }
    }
    await this.syncPublicGroups(groups);
    await this.importHistory();
    let after: string | null = null;
    do {
      const page: ResponseMap["mls.history.wanted"] = await this.request("mls.history.wanted", { after });
      for (const wanted of page.wanted) await this.queueHistory(wanted.group, wanted.device, wanted.assignment ?? null);
      after = page.next;
    } while (after);
    await this.sendHistory();
    await this.requestMissing();
  }
  /** Follows the restarts the server holds (docs/e2e-build.md "Restarts")
   * and returns the conversations to leave alone in this pass: those whose
   * server holds an incarnation older than this device's live pin, or a
   * sibling of it (it lost a restart: never replaced, never restarted from
   * here), and those whose chain could not be accepted yet. */
  private async acceptRestarts(groups: MlsGroup[]): Promise<Set<string>> {
    const held = new Set<string>();
    const pins = new Map(this.inventory().filter(g => g.live).map(g => [g.conversation, g]));
    for (const remote of groups) {
      const pin = pins.get(remote.conversation);
      if (pin ? encoded(pin.group) === remote.group : remote.incarnation === 0) continue;
      if (!pin || Number(pin.incarnation) < remote.incarnation) {
        try {
          if (this.due(remote.conversation)) await this.acceptChain(remote.conversation, pin);
        } catch (error) {
          this.assertCurrent(this.activeGeneration);
          await this.engine.reload();
          this.failed(remote.conversation, error);
        }
        const live = this.livePin(remote.conversation);
        if (live && encoded(live.group) === remote.group) continue;
      }
      held.add(remote.conversation);
    }
    return held;
  }
  /** Validates the chain from this device's live pin, or from incarnation zero without one. */
  private async acceptChain(conversation: string, pin: GroupSummary | undefined): Promise<void> {
    const chain: MlsIncarnation[] = [];
    // The cursor is exclusive: start at the pin, which anchors the chain.
    let after: string | null = pin && pin.incarnation !== "0" ? encodeBase64Url(new TextEncoder().encode(String(BigInt(pin.incarnation) - 1n))) : null;
    do {
      const page: ResponseMap["mls.group_history"] = await this.request("mls.group_history", { conversation, after });
      chain.push(...page.incarnations); after = page.next;
    } while (after);
    const links = chain.map(i => ({ group: bytes(i.group), descriptor: bytes(i.descriptor) }));
    try {
      await this.transition({ accept_chain: { server: this.server, conversation, chain: links } });
    } catch (error) {
      if (!(error instanceof AgentOperationError) || !String(error.message).includes(PRIVATE_CHAIN)) throw error;
      await this.engine.reload();
      // A private conversation: adopted only once its final successor gives current authority
      // (staged adoption, 6c); otherwise it stays pending and the device waits for a manager.
      const tip = chain[chain.length - 1]!.group;
      const { info, tree } = await this.groupInfo(tip);
      const result = await this.transition<{ group: number[]; adopted: boolean }>({ accept_private_chain: { server: this.server, conversation, chain: links, group_info: info, tree, now: this.now(), revocations: this.options.revocations() } });
      if (!result.adopted) { await this.offerAccess(tip, info, tree); throw new Error(WAITING, { cause: error }); }
    }
    this.triggers.delete(conversation);
  }
  /** The server showed this device's live pin behind its durable checkpoint.
   * Two such checks against the same pin and checkpoint, at least a minute
   * apart, restart the conversation (and throw Restarted); a response at or
   * after the checkpoint cancels them. */
  private async observeBehind(local: GroupSummary): Promise<void> {
    const checkpoint = local.record.checkpoint;
    if (!local.live || !checkpoint) return;
    const key = `${encoded(local.group)}/${checkpoint.epoch}/${encoded(checkpoint.hash)}`;
    const first = this.triggers.get(local.conversation);
    if (first?.key !== key) this.triggers.set(local.conversation, { key, at: this.now() });
    else if (this.now() - first.at >= RESTART_AFTER) {
      this.triggers.delete(local.conversation);
      await this.createGroup(local.conversation, local.history, { incarnation: Number(local.incarnation) + 1, previous: encoded(local.group) });
      // Not a failure, whichever call made it: no backoff leaves the new group behind.
      this.groupDue.delete(local.conversation);
      throw new Restarted();
    }
    this.wakeAt = Math.min(this.wakeAt ?? Infinity, this.triggers.get(local.conversation)!.at + RESTART_AFTER);
  }
  private due(conversation: string): boolean {
    const at = this.groupDue.get(conversation)?.at ?? 0;
    if (at <= this.now()) return true;
    this.wakeAt = Math.min(this.wakeAt ?? Infinity, at); return false;
  }
  private failed(conversation: string, error: unknown): void {
    const delay = Math.min(10 * 60_000, 2 * (this.groupDue.get(conversation)?.delay ?? 5_000));
    this.groupDue.set(conversation, { at: this.now() + delay * (0.5 + Math.random() / 2), delay });
    this.options.onError?.(error);
  }
  /** Public groups (docs/e2e-build.md "Groups"): forget deleted channels'
   * groups, keep joined ones current, join the rest one at a time with jitter,
   * create missing ones after a random delay, and update stale own leaves. A
   * failing group backs off without blocking direct messages. */
  private async syncPublicGroups(groups: MlsGroup[]): Promise<void> {
    const channels = this.channels;
    if (!channels) return;
    for (const g of this.inventory()) {
      const id = channelOf(g.conversation);
      // Tombstones stay: they are all that is left of a forgotten group.
      if (id === undefined || channels.has(id) || g.record.status === "forgotten") continue;
      const group = encoded(g.group);
      // The group's runtime jobs go with it, in the same write.
      const jobs = [...this.engine.jobs.values()].filter(job => job.kind !== "packages" && job.server === this.server && job.group === group);
      await this.engine.op({ forget: this.args(group) }, () => jobs.map(job => jobChange(job, true)));
      for (const job of jobs) this.historyDue.delete(jobKey(job));
      this.missing.delete(group); this.statuses.delete(g.conversation);
      this.syncedVersions.delete(group); this.groupVersions.delete(group);
    }
    let leafUpdated = false;
    const conversations = [...channels].map(([id, history]) => [`channel:${id}`, history] as [string, boolean]);
    if (this.serverChat) conversations.push(["server", true]);
    for (const [conversation, history] of conversations) {
      if (this.held.has(conversation)) continue;
      const remote = groups.find(g => g.conversation === conversation);
      const local = remote && this.summary(remote.group);
      const member = local && ["active", "joining"].includes(local.record.status);
      if (!this.due(conversation)) continue;
      try {
        if (!remote) {
          if (this.inventory().some(g => g.conversation === conversation && g.record.status === "creating")) continue;
          // Every member's device may find the group missing: one random wait
          // lets the first creation reach the others before they try.
          // A private channel's group is made by its creator with the members it chose; others
          // (an authority) only after a minute without one, admitting authorities alone.
          const wait = this.isPrivate(conversation) ? 60_000 : 5_000;
          if (!this.groupDue.has(conversation)) { this.groupDue.set(conversation, { at: this.now() + Math.random() * wait, delay: wait }); this.due(conversation); continue; }
          if (this.isPrivate(conversation) && !this.pendingMembers.has(conversation) && !this.isAuthority()) continue;
          await this.createGroup(conversation, history);
        } else if (member) {
          if (!this.current(remote)) await this.syncGroup(remote.group, remote.pending);
          const own = this.summary(remote.group)?.record.own_commit_at ?? 0;
          if (!leafUpdated && this.now() - own > LEAF_UPDATE_AFTER && !this.outbox(remote.group)) {
            leafUpdated = true; // One group at a time.
            await this.engine.op({ commit: { ...this.args(remote.group), adds: [], removes: [], now: this.now(), revocations: this.options.revocations() } });
            await this.syncGroup(remote.group);
          }
        } else {
          if (this.nextJoin > this.now()) { this.wakeAt = Math.min(this.wakeAt ?? Infinity, this.nextJoin); continue; }
          this.nextJoin = this.now() + Math.random() * 2_000;
          await this.syncGroup(remote.group, remote.pending);
        }
        this.groupDue.delete(conversation);
        this.waiting.delete(conversation);
        if (remote) await this.requestHistory(remote.group);
      } catch (error) {
        this.assertCurrent(this.activeGeneration);
        if (error instanceof Restarted) continue;
        if (String(error).includes(WAITING)) this.waiting.add(conversation);
        // Durable state is authoritative: drop whatever the failure left in memory.
        await this.engine.reload();
        this.failed(conversation, error);
      }
    }
  }
  /** Nothing to do for this group: no event since its last synchronization,
   * the server's epoch is this device's, and nothing waits to be committed. */
  private current(remote: MlsGroup): boolean {
    const local = this.summary(remote.group), gate = this.gate(remote.group);
    return !remote.pending && local?.record.checkpoint?.epoch === String(remote.epoch) && gate.status === "active"
      && !gate.rejoin && !gate.server_pending && !gate.blocked.length && !gate.pending_commit && !repairable(gate)
      && !this.offers.has(remote.group) && !this.renewalDue(remote.group, gate)
      && this.syncedVersions.has(remote.group) && this.syncedVersions.get(remote.group) === (this.groupVersions.get(remote.group) ?? 0);
  }
  /** After joining, ask the server to route history to this device; senders
   * decide by their evidence. The intent is durable (written with the join)
   * and is kept until a request succeeds. */
  private async requestHistory(group: string): Promise<void> {
    const job: RequestJob = { kind: "request", server: this.server, group };
    if (!this.engine.jobs.has(jobKey(job)) || this.summary(group)?.record.status !== "active") return;
    const next = (this.lastRequest.get(group) ?? -Infinity) + 60_000;
    if (next > this.now()) { this.wakeAt = Math.min(this.wakeAt ?? Infinity, next); return; }
    try { await this.request("mls.history.request", { group }); }
    catch (error) {
      if (!(error instanceof RequestError) || error.code !== "rate_limited") throw error;
      this.lastRequest.set(group, this.now()); return;
    }
    this.lastRequest.set(group, this.now());
    await this.engine.saveJobs([jobChange(job, true)]);
  }
  /** Lets this client create its own new channel's group right away. */
  channelCreated(id: number, members?: string[]): void {
    if (members) { this.privateChannels.add(id); this.pendingMembers.set(`channel:${id}`, members); }
    this.groupDue.set(`channel:${id}`, { at: 0, delay: 5_000 }); this.schedule();
  }
  /** A private channel's first members: those chosen in the interface, the predecessor's on a
   * restart, or none (authorities are always admitted). */
  private initialMembers(conversation: string, successor?: Successor): string[] {
    const chosen = this.pendingMembers.get(conversation);
    if (chosen) return chosen;
    if (!successor || this.summary(successor.previous)?.record.status !== "active") return [];
    return this.engine.query<AccessView | null>({ access_view: this.args(successor.previous) })?.members ?? [];
  }
  private isPrivate(conversation: string): boolean {
    const id = channelOf(conversation);
    return id !== undefined && this.privateChannels.has(id);
  }
  private isAuthority(): boolean {
    const me = this.uid, view = this.owner;
    return !!view && (view.owner === me || view.confirmers.includes(me) || view.managers.includes(me));
  }
  /** An encrypted private channel's member list as this device holds it (6c). */
  privateMembers(channel: number): AccessView | null {
    const pin = this.livePin(`channel:${channel}`);
    return pin?.private && pin.record.status === "active" ? this.engine.query<AccessView | null>({ access_view: this.args(encoded(pin.group)) }) : null;
  }
  /** Signs a new member list for an encrypted private channel and commits it with the removals it
   * requires (6c, "Who commits"). This device's user must be the owner, a confirmer or a manager. */
  async setPrivateMembers(channel: number, members: string[]): Promise<void> {
    return this.engine.run(async () => {
      const generation = this.generation; this.activeGeneration = generation; this.assertCurrent(generation);
      const pin = this.livePin(`channel:${channel}`);
      if (!pin?.private) throw new Error("not an encrypted private channel");
      const group = encoded(pin.group);
      await this.syncGroup(group);
      await this.engine.op({ commit_access: { ...this.args(group), members, now: this.now(), revocations: this.options.revocations() } });
      await this.syncGroup(group);
    });
  }
  /** The ratchet tree of a public group's current epoch, which its GroupInfos
   * leave out (docs/e2e-build.md "Large groups"). */
  private async tree(group: string): Promise<{ epoch: number; tree: number[] }> {
    const origin = this.options.link.endpoint ? httpOriginFromWsUrl(this.options.link.endpoint) : null;
    if (!origin || !this.fileAccess) throw new Error("MLS tree unavailable");
    const generation = this.generation, download = new AbortController();
    const timeout = setTimeout(() => download.abort(new Error("MLS tree request timed out")), TREE_TIMEOUT_MS);
    const stop = () => download.abort(this.options.signal?.reason);
    this.options.signal?.addEventListener("abort", stop, { once: true });
    this.downloads.add(download);
    try {
      const response = await (this.options.fetch ?? fetch)(`${origin}/api/mls/${group}/tree`, { headers: { Authorization: `Bearer ${this.fileAccess}` }, cache: "no-store", signal: download.signal });
      if (!response.ok) throw new Error(`MLS tree request failed (${response.status})`);
      const epoch = Number(response.headers.get("x-gwar-epoch") ?? NaN);
      if (!Number.isSafeInteger(epoch) || epoch < 0) throw new Error("MLS tree epoch");
      if (Number(response.headers.get("content-length") ?? 0) > MAX_TREE_BYTES) throw new Error("MLS tree too large");
      // Read up to the limit, never more.
      const reader = response.body?.getReader();
      if (!reader) throw new Error("MLS tree response has no body");
      const tree: number[] = [];
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (tree.length + value.byteLength > MAX_TREE_BYTES) { await reader.cancel(); throw new Error("MLS tree too large"); }
        for (const byte of value) tree.push(byte);
      }
      this.assertCurrent(generation);
      return { epoch, tree };
    } finally {
      // Also ends a body left unread when the headers were refused.
      clearTimeout(timeout); download.abort(); this.downloads.delete(download);
      this.options.signal?.removeEventListener("abort", stop);
    }
  }
  /** A group's GroupInfo, with the tree of the same epoch for public groups. */
  private async groupInfo(group: string): Promise<{ info: number[]; tree: number[] | null }> {
    const conversation = this.summary(group)?.conversation ?? this.conversations.get(group);
    // A commit between the two requests changes the epoch: ask again.
    for (let attempt = 0; attempt < 3; attempt++) {
      const info = await this.request("mls.group_info", { group });
      if (conversation?.startsWith("dm:")) return { info: bytes(info.group_info), tree: null };
      const tree = await this.tree(group);
      if (tree.epoch === info.epoch) return { info: bytes(info.group_info), tree: tree.tree };
    }
    throw new Error("MLS group changed while joining");
  }
  private async join(group: string): Promise<void> {
    const { info, tree } = await this.groupInfo(group);
    const local = this.summary(group);
    if (local?.live && local.record.checkpoint) {
      if (this.engine.query<boolean>({ behind: { server: this.server, group: bytes(group), bytes: info, tree } })) {
        await this.observeBehind(local);
        throw new Error("The server is behind this device in this conversation");
      }
      // A verified GroupInfo at or past the checkpoint: no restart is due.
      this.triggers.delete(local.conversation);
    }
    try {
      await this.engine.op<CommitOutbox>({ join_external: { server: this.server, bytes: info, tree, now: this.now(), revocations: this.options.revocations() } }, out => {
        if (encoded(out.group) !== group) throw new Error("GroupInfo group mismatch");
        const conversation = this.summary(group)?.conversation ?? "";
        return conversation === "server" || channelOf(conversation) !== undefined ? [jobChange({ kind: "request", server: this.server, group })] : [];
      });
    } catch (error) {
      // A private group whose lists give no current authority: the device waits for a channel
      // manager (retrying after managers.updated, an offer, or its backoff). An authority that
      // cannot join offers fresh lists for a member to commit, once per GroupInfo (6c, "Offers").
      if (error instanceof AgentOperationError && String(error.message).includes(WAITING)) {
        await this.engine.reload();
        await this.offerAccess(group, info, tree);
      }
      throw error;
    }
    // A rejoined parent may lack devices the voice call still shares keys with.
    this.voiceGate(group);
  }
  private async syncGroup(group: string, pending = false, beforeWelcome = false): Promise<boolean | "pruned"> {
    // Every path that replays a private group first records the access it holds (an interrupted
    // observation included); a failure to persist it aborts the replay.
    await this.observeAccess(group);
    // Concurrent joins can lose. Bounded retries yield to backoff instead of
    // holding the device's queue forever on an adversarial/busy server.
    let pages = 0;
    groupLoop: for (let attempt = 0; attempt < 8; attempt++) {
      if (++pages > 128) throw new Error("MLS catch-up continues on the next pass");
      const version = this.groupVersions.get(group) ?? 0;
      let local = this.summary(group);
      // A tombstone (the channel was hidden and is shown again) is joined like a removal.
      if (!local || local.record.status === "removed" || local.record.status === "rejoin" || local.record.status === "forgotten") {
        if (beforeWelcome) return false;
        await this.join(group); local = this.summary(group)!;
      }
      if (local.record.status === "creating") return false;
      const out = this.outbox(group);
      const since = wireEpoch(out?.epoch ?? local.record.checkpoint!.epoch);
      const log = await this.request("mls.log", { group, since, limit: 100 });
      if (log.head < since) {
        if (beforeWelcome) return false;
        await this.observeBehind(local);
        throw new Error("The server is behind this device in this conversation");
      }
      if (log.oldest > since) {
        if (beforeWelcome) return "pruned";
        if (out) { await this.transition({ reconcile: { ...this.args(group), result: { case: "pruned", oldest: String(log.oldest) } } }); await this.observeAccess(group); this.voiceGate(group); }
        else await this.join(group); // Validated newer GroupInfo replaces pruned membership.
        continue;
      }
      if (out) {
        const entry = log.entries.find(e => e.epoch === since);
        if (entry) {
          try {
            const resolution = await this.transition<Resolution>({ reconcile: { ...this.args(group), result: { case: "entry", epoch: String(entry.epoch), commit: bytes(entry.commit), proposals: entry.proposals.map(bytes) } } });
            await this.observeAccess(group);
            this.voiceGate(group);
            // A validated entry continuing from the checkpoint: no restart is
            // due. A pending join that lost is dropped without validating it.
            if (resolution !== "rejoin" || local.record.status !== "joining") this.triggers.delete(local.conversation);
          } catch (error) {
            if (!(error instanceof AgentOperationError)) throw error;
            await this.engine.reload();
            if (beforeWelcome) return false;
            // The core permits replacing this outbox only with a validated
            // GroupInfo strictly newer than its source epoch.
            await this.join(group);
          }
          continue;
        }
        if (log.head !== since) throw new Error("MLS log omitted pending commit epoch");
        if (beforeWelcome) return false;
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
          await this.observeAccess(group);
          this.voiceGate(group);
        } catch (error) {
          if (!(error instanceof AgentOperationError)) throw error;
          // Expired Add packages can make an old log unreplayable. Discard the
          // failed agent, then validate a fresh GroupInfo and replace membership.
          await this.engine.reload();
          if (beforeWelcome) return false;
          await this.join(group);
          continue groupLoop;
        }
        expected++;
        this.triggers.delete(local.conversation);
        if (this.summary(group)?.record.status !== "active") break;
      }
      if (this.summary(group)?.record.status !== "active") continue;
      if (expected < log.head || log.next !== null) {
        if (expected === since) throw new Error("MLS log made no progress");
        // Catch-up pages do not consume the contention retry budget.
        attempt--; continue;
      }
      if (expected !== log.head) throw new Error("MLS log head mismatch");
      if (beforeWelcome) return true;
      if (pending || log.pending.length) await this.engine.op({ pending: { ...this.args(group), pending: true } });
      for (const proposal of log.pending) { await this.engine.op({ proposal: { ...this.args(group), bytes: bytes(proposal) } }); this.voiceGate(group); }
      await this.engine.op({ pending: { ...this.args(group), pending: false } });
      pending = false;
      const gate = this.gate(group);
      if (gate.rejoin) { await this.join(group); continue; }
      if (gate.server_pending || gate.blocked.length) {
        await this.engine.op({ commit: { ...this.args(group), adds: [], removes: [], now: this.now(), revocations: this.options.revocations() } });
        continue;
      }
      if (await this.repairAccess(group, gate)) continue;
      if (local.conversation.startsWith("dm:") && await this.admitMissing(group)) continue;
      if (version !== (this.groupVersions.get(group) ?? 0)) continue;
      this.syncedVersions.set(group, version);
      return true;
    }
    throw new Error("MLS group is still securing; retrying");
  }
  /** A direct message's participants' devices, or one uid's devices in a channel. */
  private async devices(conversation: string, uid?: string): Promise<MlsDevice[]> {
    const uids = conversation.startsWith("dm:") ? participants(conversation) : [uid!];
    const all: MlsDevice[] = []; let after: string | null = null;
    do { const page: ResponseMap["mls.devices"] = await this.request("mls.devices", { uids, after }); all.push(...page.devices); after = page.next; } while (after);
    return all;
  }
  private async currentDevices(devices: MlsDevice[]): Promise<MlsDevice[]> {
    const revoked = await Promise.all(this.options.revocations().filter(r => r.revoked_at <= this.now()).map(async r => ({ device: r.device_key, uid: await uidForPublicKey(r.account_key) })));
    return devices.filter(d => !revoked.some(r => r.device === d.device_key && r.uid === d.uid));
  }
  private async admitMissing(group: string): Promise<boolean> {
    const local = this.summary(group)!;
    const members = new Set(local.members.map(m => m.device_key));
    const devices = await this.currentDevices(await this.devices(local.conversation));
    if (!devices.some(d => !members.has(d.device_key))) return false;
    let after: string | null = null;
    const claimed: { device_key: string; package: string }[] = [];
    do {
      const page: ResponseMap["mls.key_packages.claim"] = await this.request("mls.key_packages.claim", { uids: participants(local.conversation), after });
      claimed.push(...page.packages);
      after = page.next;
    } while (after);
    // The local feed may advance while claims are in flight. Filter again.
    const candidates = new Set((await this.currentDevices(devices)).filter(d => !members.has(d.device_key)).map(d => d.device_key));
    const adds = claimed.filter(p => candidates.has(p.device_key)).map(p => bytes(p.package));
    if (!adds.length) return false; // A device with no package joins itself on connect.
    await this.engine.op({ commit: { ...this.args(group), adds, removes: [], now: this.now(), revocations: this.options.revocations() } });
    return true;
  }
  private async queueHistory(group: string, recipient: string, assignment: string | null): Promise<void> {
    const local = this.summary(group);
    if (!local || !this.enabled(local.conversation) || !local.members.some(m => m.device_key === recipient) || recipient === this.options.identity.publicKey) return;
    const job: HistoryJob = { kind: "history", server: this.server, group, recipient, publication: null, chunks: null, assignment };
    const queued = this.engine.jobs.get(jobKey(job)) as HistoryJob | undefined;
    // A queued answer takes the newest assignment; its chunks stay as they are.
    if (!queued) await this.engine.saveJobs([jobChange(job)]);
    else if (assignment && queued.assignment !== assignment) await this.engine.saveJobs([jobChange({ ...queued, assignment })]);
  }
  private assertGroupSynced(group: string): void {
    if (this.syncedVersions.get(group) !== (this.groupVersions.get(group) ?? 0)) throw new GroupChanged();
  }
  /** Whether this device may share a group's history now: its gate is clear
   * and its conversation is not held for a restart. */
  private mayShare(group: string): boolean {
    const conversation = this.summary(group)?.conversation;
    if (!this.enabled(conversation)) return false;
    const gate = this.gate(group);
    return usable(gate) && conversation !== undefined && !this.held.has(conversation);
  }
  private async sendHistory(): Promise<void> {
    jobs: for (const value of [...this.engine.jobs.values()]) {
      if (value.kind !== "history" || value.server !== this.server) continue;
      let job = value;
      if (this.syncedVersions.get(job.group) !== (this.groupVersions.get(job.group) ?? 0)) continue;
      const local = this.summary(job.group);
      if (!this.enabled(local?.conversation)) continue;
      if (!local?.members.some(m => m.device_key === job.recipient)) {
        await this.engine.saveJobs([jobChange(job, true)]); continue;
      }
      if (!this.mayShare(job.group)) continue;
      const due = this.historyDue.get(jobKey(job));
      if (due && due.at > this.now()) { this.wakeAt = Math.min(this.wakeAt ?? Infinity, due.at); continue; }
      try {
        const uid = local.members.find(m => m.device_key === job.recipient)!.uid;
        const recipient = (await this.devices(local.conversation, uid)).find(d => d.device_key === job.recipient && d.uid === uid);
        if (!recipient) continue;
        if (job.chunks === null || (job.publication && recipient.created_at > job.publication.created_at)) {
          const publication = { device_key: recipient.device_key, hpke_key: recipient.hpke_key, created_at: recipient.created_at, signature: recipient.signature };
          await this.engine.op<number[][]>({ share_history: { ...this.args(job.group), recipient: publication, now: this.now(), revocations: this.options.revocations() } }, chunks => {
            job = { ...job, publication, chunks: chunks.map(encoded) }; return [jobChange(job)];
          });
        }
      } catch (error) {
        // One recipient's bad publication or a failed lookup must not starve
        // the other jobs: this one backs off (5 seconds doubling to 10 minutes).
        if (!(error instanceof RequestError) && !(error instanceof AgentOperationError)) throw error;
        await this.engine.reload(); this.options.onError?.(error);
        const delay = Math.min(10 * 60_000, 2 * (due?.delay ?? 2_500));
        this.historyDue.set(jobKey(job), { at: this.now() + delay, delay }); continue;
      }
      this.historyDue.delete(jobKey(job));
      if (!job.chunks!.length && job.assignment) {
        // Nothing this device may share: let the server ask another.
        try { await this.request("mls.history.decline", { group: job.group, device: job.recipient, assignment: job.assignment }); }
        catch (error) { if (!(error instanceof RequestError)) throw error; this.options.onError?.(error); }
      }
      for (const chunk of job.chunks!) {
        if (!this.mayShare(job.group)) continue jobs;
        try { await this.request("mls.history.share", { group: job.group, recipient: job.recipient, chunk, assignment: job.assignment ?? null }, () => {
          this.assertGroupSynced(job.group);
          if (!this.mayShare(job.group)) throw new Error("history sharing blocked");
        }); }
        catch (error) {
          // The group changed while preparing: the next pass syncs it and retries the job.
          if (error instanceof GroupChanged) continue jobs;
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
        const local = this.summary(chunk.group);
        if (local?.record.status !== "active" || !this.enabled(local.conversation)) continue;
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
      const local = this.summary(group);
      if (!this.enabled(local?.conversation)) continue;
      for (const epoch of epochs) {
        const record = this.engine.query<null | string | { derived?: unknown; imported?: unknown }>({ epoch: { ...this.args(group), epoch } });
        if (record && record !== "conflicted") epochs.delete(epoch);
      }
      if (!epochs.size) { this.missing.delete(group); continue; }
      if (this.now() - (this.lastRequest.get(group) ?? -Infinity) < 60_000) continue;
      if (local?.record.status !== "active" || this.held.has(local.conversation)) continue;
      try { await this.request("mls.history.request", { group }); }
      catch (error) { if (!(error instanceof RequestError) || error.code !== "rate_limited") throw error; }
      this.lastRequest.set(group, this.now());
    }
  }
  async openConversation(conversation: string): Promise<void> {
    participants(conversation);
    return this.engine.run(async () => {
      this.activeGeneration = this.generation; this.assertCurrent(this.activeGeneration);
      this.statuses.set(conversation, "creating"); this.notify();
      const known = (await this.groups()).find(g => g.conversation === conversation);
      for (const g of this.inventory()) if (g.conversation === conversation && g.record.status === "creating") {
        await this.transition({ settle: { ...this.settleArgs(encoded(g.group)), winner: known ? bytes(known.group) : null } });
      }
      if (known && (await this.acceptRestarts([known])).size) { this.held.add(conversation); throw new Error("MLS conversation is still securing"); }
      if (known) { this.held.delete(conversation); await this.syncGroup(known.group, known.pending); this.updateStatuses(); return; }
      await this.createGroup(conversation, true);
      await this.sendHistory();
      this.updateStatuses();
    }).catch(error => { this.markSecuring(); this.schedule(1000); throw error; });
  }
  /** Creates the conversation's group, or a restart's next incarnation, and
   * settles with the group the server holds. */
  private async createGroup(conversation: string, history: boolean, successor?: Successor): Promise<void> {
    // A private channel's group carries its lists (6c): the founding owner (the same along the
    // chain), and the chosen members, the predecessor's on a restart, or none (authorities only).
    const isPrivate = this.isPrivate(conversation);
    const founding = isPrivate ? this.livePin(conversation)?.founding_owner ?? this.owner?.owner : undefined;
    if (isPrivate && !founding) throw new Error(WAITING);
    const d = await descriptor(this.options.identity, this.uid, this.server, conversation, this.now(), history, successor, founding);
    const info = isPrivate
      ? await this.engine.op<number[]>({ create_private: { descriptor: d.bytes, members: this.initialMembers(conversation, successor), now: this.now(), revocations: this.options.revocations() } })
      : await this.engine.op<number[]>({ create: { descriptor: d.bytes } });
    this.pendingMembers.delete(conversation);
    let winner: string;
    try { winner = (await this.request("mls.group.create", { conversation, group_info: encoded(info) })).group; }
    catch (error) {
      // A restart the server refuses (it holds another chain) settles with what it holds.
      if (!(error instanceof RequestError) || (error.code !== "conflict" && !(successor && error.code === "bad_request"))) throw error;
      // Conflict text is not a typed protocol field. Resolve through groups.
      const found = (await this.groups()).find(g => g.conversation === conversation);
      if (!found) throw error;
      winner = found.group;
    }
    await this.transition({ settle: { ...this.settleArgs(d.group), winner: bytes(winner) } });
    // Another device's restart is joined after its chain is accepted, on the next pass.
    if (successor && winner !== d.group) { this.schedule(); return; }
    await this.syncGroup(winner);
  }
  /** Signing runs inside the queue, with the exact current group/epoch. The
   * builder may await WebCrypto; close waits for it and its write to finish. */
  async sealMessage(conversation: string, build: (context: SealContext) => Promise<Plaintext>): Promise<{ group: string; epoch: number; ciphertext: string }> {
    return this.engine.run(async () => {
      const generation = this.generation; this.activeGeneration = generation; this.assertCurrent(generation);
      if (!this.enabled(conversation)) throw new Error("Server chat is not encrypted");
      const local = this.livePin(conversation);
      if (!local || this.held.has(conversation)) throw new Error("open the conversation first");
      const group = encoded(local.group);
      await this.syncGroup(group);
      const [epoch] = this.engine.query<[string, number[]]>({ sending_key: { ...this.args(group), now: this.now(), revocations: this.options.revocations() } });
      const plaintext = await build({ server_key: this.server, conversation, group_id: group, epoch });
      this.assertCurrent(generation); this.assertGroupSynced(group);
      const ciphertext = await this.engine.op<number[]>({ seal_message: { context: { server_key: this.server, conversation, group_id: bytes(group), epoch, message_id: [...decodeFixed(plaintext.signature.message_id, 16)], revision: plaintext.signature.revision }, plaintext: [...new TextEncoder().encode(JSON.stringify(plaintext))], now: this.now(), revocations: this.options.revocations() } });
      this.assertCurrent(generation); this.assertGroupSynced(group);
      return { group, epoch: wireEpoch(epoch), ciphertext: encoded(ciphertext) };
    });
  }
  /** Read-only authentication. Operational failures reject; UI continuity and
   * revocation policy remain the caller's responsibility. */
  async openMessage(message: SealedFields): Promise<OpenResult> {
    return this.engine.run(async () => {
      if (!this.enabled(message.conversation)) return { reason: "cannot_open" };
      let context, ciphertext;
      try {
        if (!Number.isInteger(message.revision) || message.revision < 0 || message.revision > 0xffffffff || typeof message.conversation !== "string" || /[\r\n]/.test(message.conversation)) return { reason: "cannot_open" };
        context = { server_key: this.server, conversation: message.conversation, group_id: [...decodeFixed(message.group, 32)], epoch: String(wireEpoch(message.epoch)), message_id: [...decodeFixed(message.message_id, 16)], revision: message.revision };
        ciphertext = bytes(message.ciphertext);
      } catch { return { reason: "cannot_open" }; }
      const result = this.engine.query<{ ok: number[] } | { reason: "no_key" | "cannot_open" }>({ open_message: { context, ciphertext } });
      if ("ok" in result) {
        const plaintext = await authenticatePlaintext(new Uint8Array(result.ok), { ...context, group_id: message.group, message_id: message.message_id });
        return plaintext ? { ok: plaintext } : { reason: "cannot_open" };
      }
      if (result.reason === "no_key") {
        const epochs = this.missing.get(message.group) ?? new Set<string>();
        const newDemand = !epochs.has(context.epoch);
        epochs.add(context.epoch); this.missing.set(message.group, epochs);
        // Subscribers retry unopened rows after sync. Repeated demand must not
        // turn that notification into an immediate synchronization loop.
        if (newDemand) this.schedule();
      }
      return result;
    });
  }
  /** Starts the controller of an encrypted call this session just entered.
   * The caller closes it on leave, move or a different call. */
  async voiceCall(state: VoiceState, session: number, restartMedia: () => void): Promise<VoiceCall> {
    if (this.stopped || !this.options.link.isOnline) throw new Error("MLS runtime unavailable");
    // A reconnect makes every capture of this call stale.
    const generation = this.generation;
    const factory = await loadVoiceAgent(this.options.seed, credential(this.options.identity), this.now);
    this.assertCurrent(generation);
    const call: VoiceCall = new VoiceCall({
      server: this.server, uid: this.uid, session, identity: this.options.identity,
      request: (op, data, guard) => this.request(op, data, guard, generation),
      tree: group => { this.assertCurrent(generation); return this.tree(group); },
      revocations: () => this.options.revocations(),
      now: () => this.now(),
      monotonic: () => performance.now(),
      restartMedia,
      parent: this.engine,
      parentGroup: channel => {
        const pin = this.livePin(`channel:${channel}`);
        return pin && pin.record.status === "active" ? encoded(pin.group) : null;
      },
      parentClean: group => {
        const gate = this.gate(group);
        // An event announced a parent change this device has not processed yet.
        const unseen = this.syncedVersions.get(group) !== (this.groupVersions.get(group) ?? 0);
        return !unseen && usable(gate) && !this.outbox(group);
      },
      syncParent: group => this.engine.run(async () => {
        this.activeGeneration = this.generation; this.assertCurrent(this.activeGeneration);
        await this.syncGroup(group);
        const gate = this.gate(group);
        return usable(gate) && !this.outbox(group);
      }),
      factory,
      onError: error => this.options.onError?.(error),
    }, state);
    this.calls.add(call);
    call.start();
    return call;
  }
  /** An accepted parent change closes affected voice gates at once, before
   * this pass continues with further network waits. */
  private voiceGate(group: string): void {
    for (const call of this.calls) if (call.channelGroup === group) call.recheck();
  }
  /** Ends a call's controller: its pin, agent, timers and pending results. */
  endVoiceCall(call: VoiceCall): void {
    call.close(); this.calls.delete(call);
  }
  close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    // Voice closes before the lease is released (a takeover included).
    for (const call of this.calls) call.close();
    this.calls.clear();
    this.stopped = true; this.generation++;
    clearTimeout(this.retry);
    for (const download of this.downloads) download.abort();
    for (const unsubscribe of this.unsubscribers) unsubscribe();
    // A link that never connected stays usable for its owner.
    if (this.options.link.status.state !== "idle") this.options.link.close();
    this.closePromise = (async () => {
      try { await this.engine.close(); } finally { await this.release(); }
      this.markSecuring();
    })();
    return this.closePromise;
  }
}
