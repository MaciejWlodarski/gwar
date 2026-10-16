/**
 * The volatile MLS-to-media controller of one encrypted call (docs/e2e-build.md
 * "Calls and membership", "Wire and delivery" and "Installation and
 * activation"). It owns a memory-only voice agent under the device lease,
 * joins or creates the call's group, follows its log, and turns processed
 * epochs into hold/supersede/install/activate controls for the run's SFrame
 * engine. Key material only goes to that engine, never to state or logs.
 */
import type { Identity } from "../net/identity";
import { RequestError } from "../net/connection";
import { decodeFixed } from "../net/handshake";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import type { Op, RequestInput, ResponseMap } from "../net/protocol";
import type { VoiceState } from "../proto/VoiceState";
import type { Control, ControlResult, CryptoStatus, VoiceKeyBinding } from "../voice/sframe/state";
import { voiceDescriptor } from "./descriptor";
import { AgentOperationError, Engine } from "./engine";
import { memoryStorage } from "./storage";
import type { AgentFactory, CommitOutbox, Gate, GroupSummary, Resolution, Revocation } from "./types";

const bytes = (value: string): number[] => [...decodeBase64Url(value)];
const encoded = (value: number[]): string => encodeBase64Url(new Uint8Array(value));
function wireEpoch(value: bigint | string): number {
  const n = Number(value);
  if (!Number.isSafeInteger(n) || n < 0 || String(n) !== String(value)) throw new Error("epoch outside delivery protocol range");
  return n;
}

/** Acknowledgements wait at most this long before a non-removal epoch activates. */
export const ACK_WAIT_MS = 2_000;
const FUTURE_ACK_EPOCHS = 8, FUTURE_ACK_DEVICES = 1000, FUTURE_ACK_MS = 10_000, FUTURE_ACK_AHEAD = 64n;
const REPEAT_EVERY_MS = 6_000;

export type VoiceCallStatus = "securing" | "active" | "failed";
/** One media run's engine: it applies controls and reports its status. */
export interface VoiceMedia { run: number; control(body: Control): Promise<ControlResult> }
export interface VoiceRosterEntry { uid: string; devices: string[] }

export interface VoiceHost {
  server: string;
  uid: string;
  session: number;
  identity: Identity;
  request<O extends Op>(op: O, data: RequestInput<O>): Promise<ResponseMap[O]>;
  tree(group: string): Promise<{ epoch: number; tree: number[] }>;
  revocations(): Revocation[];
  now(): number;
  /** The chat agent, holding the call's parent channel group. */
  parent: Engine;
  /** The live, locally accepted group of `channel:{id}`, or null. */
  parentGroup(channel: number): string | null;
  /** Synchronizes the parent group; true when it has no outstanding changes. */
  syncParent(group: string): Promise<boolean>;
  factory: AgentFactory;
  onError(error: unknown): void;
}

interface Media extends VoiceMedia {
  serial: bigint;
  barrier: bigint | null;
  installed: bigint | null;
  /** Devices removed since the previous delivered installation. */
  removed: Set<string>;
  /** The epoch this run's own update made; installs start there (restart rule). */
  floor: bigint | null;
  activated: bigint | null;
  /** The earliest certificate expiry among the installed epoch's leaves. */
  sendUntil: number | null;
}
interface VoiceKeys { epoch: string; own_leaf: number; members: { leaf: number; device: string; key: number[] }[]; send_until: number | null }

class Closed extends Error { constructor() { super("voice call closed"); } }

export class VoiceCall {
  readonly channel: number;
  readonly call: string;
  readonly participation: string;
  readonly channelGroup: string;
  private readonly engine: Engine;
  private group: string | null;
  private closed = false;
  private running: Promise<void> | undefined;
  private dirty = true;
  private logDirty = true;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private activation: ReturnType<typeof setTimeout> | undefined;
  private statusValue: VoiceCallStatus = "securing";
  private failure: string | null = null;
  private readonly listeners = new Set<() => void>();
  /** The newest processed epoch and its validated members by device. */
  private processed: bigint | null = null;
  private processedDevices = new Set<string>();
  /** Pending(E, target, started, removal) since the last activation. */
  private pending: { started: number; removal: boolean } | null = null;
  private readonly acks = new Map<bigint, Set<string>>();
  private readonly futureAcks = new Map<bigint, { devices: Set<string>; at: number }>();
  private lostAcks = false;
  private lastRepeat = -Infinity;
  private gateBlocked = false;
  private lastExportError = "";
  private lastControl: { action: string; result: ControlResult } | null = null;
  private media: Media | null = null;
  /** Set when a new run replaces one whose sender counter is gone. */
  private needUpdate = false;
  private updateHash: string | null = null;
  private joinedResolve!: (group: string) => void;
  private joinedReject!: (error: unknown) => void;
  /** The settled group once this device's leaf is accepted. */
  readonly joined: Promise<string>;

  constructor(private readonly host: VoiceHost, state: VoiceState) {
    if (!state.encrypted || !state.channel_group) throw new Error("not an encrypted call");
    this.channel = state.channel; this.call = state.call; this.participation = state.participation;
    this.channelGroup = state.channel_group; this.group = null;
    this.engine = new Engine(host.factory, memoryStorage());
    this.joined = new Promise((resolve, reject) => { this.joinedResolve = resolve; this.joinedReject = reject; });
    this.joined.catch(() => {});
    this.initialGroup = state.group;
  }
  private initialGroup: string | null;

  status(): VoiceCallStatus { return this.statusValue; }
  /** Diagnostics without key material, for tests and support logs. */
  debug(): Record<string, unknown> {
    const m = this.media;
    return {
      group: this.group, processed: this.processed?.toString() ?? null, pending: this.pending, status: this.statusValue,
      running: this.running !== undefined, dirty: this.dirty, logDirty: this.logDirty, needUpdate: this.needUpdate, gateBlocked: this.gateBlocked,
      lastExportError: this.lastExportError, lastControl: this.lastControl, acks: [...this.acks].map(([e, d]) => [e.toString(), d.size]),
      media: m && { run: m.run, serial: m.serial.toString(), barrier: m.barrier?.toString() ?? null, installed: m.installed?.toString() ?? null, activated: m.activated?.toString() ?? null, floor: m.floor?.toString() ?? null },
    };
  }
  error(): string | null { return this.failure; }
  subscribe(listener: () => void): () => void { this.listeners.add(listener); return () => this.listeners.delete(listener); }
  private notify(): void { for (const listener of this.listeners) listener(); }
  private setStatus(status: VoiceCallStatus): void {
    if (this.statusValue === status) return;
    this.statusValue = status; this.notify();
  }

  /** Who can hear: the locally validated voice tree, by uid with every device. */
  roster(): VoiceRosterEntry[] | null {
    if (this.closed || !this.group) return null;
    let summary: GroupSummary | undefined;
    try { summary = this.summary(); } catch { return null; }
    if (summary?.record.status !== "active") return null;
    const byUid = new Map<string, string[]>();
    for (const m of summary.members) byUid.set(m.uid, [...(byUid.get(m.uid) ?? []), m.device_key]);
    return [...byUid].map(([uid, devices]) => ({ uid, devices }));
  }

  /** For a new engine: null on the call's first run; after a restart, the
   * highest processed epoch, which that engine must never install again. */
  resumeAfter(): string | null { return this.runs > 0 && this.processed !== null ? String(this.processed) : null; }

  /** A new media run. A run after another one cannot prove continuity of its
   * sender counter: it installs only from this device's next own update. */
  attach(media: VoiceMedia): void {
    if (this.closed) return;
    const restart = this.runs++ > 0;
    this.media = { ...media, serial: 0n, barrier: null, installed: null, removed: new Set(), floor: null, activated: null, sendUntil: null };
    this.setStatus("securing");
    if (restart) { this.needUpdate = true; this.updateHash = null; }
    this.schedule(true);
  }
  private runs = 0;

  /** The run's engine reported status: exhaustion or expiry needs a new own leaf. */
  engineStatus(status: CryptoStatus): void {
    if (this.closed || !this.media || status.binding.run !== this.media.run) return;
    if (status.state === "failed") { this.media = null; this.setStatus("failed"); this.failure = status.reason; return; }
    this.media.activated = null;
    this.setStatus("securing");
    // A fresh own leaf (update; an expired credential rejoins) reopens sending.
    this.needUpdate = true; this.updateHash = null;
    this.schedule(true);
  }

  /** Server events for this call; anything else is ignored. */
  event(event: { ev: string; d: unknown }): void {
    if (this.closed) return;
    const d = event.d as Record<string, unknown>;
    if (event.ev === "mls.commit" || event.ev === "mls.proposal") {
      if (d.group === this.group) this.schedule(true);
      else if (d.group === this.channelGroup) this.schedule(true);
      return;
    }
    if (event.ev === "voice.epoch.ack") {
      if (d.call !== this.call || d.group !== this.group || typeof d.epoch !== "number" || typeof d.device !== "string") return;
      this.ack(BigInt(d.epoch), d.device);
      return;
    }
    if (event.ev === "voice.epoch.request") {
      if (d.call !== this.call || d.group !== this.group || typeof d.epoch !== "number") return;
      // Only an actually installed decryptor answers.
      if (this.media?.installed === BigInt(d.epoch)) void this.sendAck(this.media, BigInt(d.epoch));
    }
  }

  private ack(epoch: bigint, device: string): void {
    if (this.processed !== null && epoch < this.processed) return;
    if (this.processed !== null && epoch === this.processed && this.media?.installed === epoch) {
      if (this.processedDevices.has(device)) { (this.acks.get(epoch) ?? this.acks.set(epoch, new Set()).get(epoch)!).add(device); this.schedule(false); }
      return;
    }
    // Before processing: hold, bounded, never creating keys.
    const now = this.host.now();
    for (const [old, entry] of this.futureAcks) if (now - entry.at > FUTURE_ACK_MS) { this.futureAcks.delete(old); this.lostAcks = true; }
    const head = this.processed ?? 0n;
    if (epoch > head + FUTURE_ACK_AHEAD) return;
    let entry = this.futureAcks.get(epoch);
    if (!entry) {
      if (this.futureAcks.size >= FUTURE_ACK_EPOCHS) {
        const oldest = [...this.futureAcks.keys()].sort((a, b) => (a < b ? -1 : 1))[0]!;
        this.futureAcks.delete(oldest); this.lostAcks = true;
      }
      entry = { devices: new Set(), at: now };
      this.futureAcks.set(epoch, entry);
    }
    if (entry.devices.size >= FUTURE_ACK_DEVICES) { this.lostAcks = true; return; }
    entry.devices.add(device);
  }

  /** Leaving, moving or losing the connection discards the pin and all state. */
  close(): void {
    if (this.closed) return;
    this.closed = true;
    clearTimeout(this.timer); clearTimeout(this.activation); clearTimeout(this.retry);
    this.media = null;
    this.acks.clear(); this.futureAcks.clear();
    this.joinedReject(new Closed());
    void this.engine.close().catch(() => {});
    this.listeners.clear();
  }

  start(): void { this.schedule(true); }

  /** Runs a pass now (or right after the current one), or after `delay`. */
  private schedule(log: boolean, delay = 0): void {
    if (this.closed) return;
    if (log) this.logDirty = true;
    if (delay > 0) {
      const at = this.host.now() + delay;
      if (this.retryAt !== undefined && this.retryAt <= at) return;
      clearTimeout(this.retry);
      this.retryAt = at;
      this.retry = setTimeout(() => { this.retry = undefined; this.retryAt = undefined; this.schedule(false); }, delay);
      return;
    }
    this.dirty = true;
    if (this.running || this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.running = this.loop().finally(() => { this.running = undefined; if (this.dirty && !this.closed) this.schedule(false); });
    }, 0);
  }
  private retry: ReturnType<typeof setTimeout> | undefined;
  private retryAt: number | undefined;

  private async loop(): Promise<void> {
    while (this.dirty && !this.closed) {
      this.dirty = false;
      try { await this.pass(); }
      catch (error) {
        if (error instanceof Closed || this.closed) return;
        this.host.onError(error);
        this.schedule(true, 2_000);
        return;
      }
    }
  }

  private assertOpen(): void { if (this.closed) throw new Closed(); }
  private args() { return { server: this.host.server, group: [...decodeFixed(this.group!, 32)] }; }
  private summary(): GroupSummary | undefined {
    return this.engine.query<GroupSummary[]>({ inventory: { server: this.host.server } }).find(g => encoded(g.group) === this.group);
  }
  private gate(): Gate { return this.engine.query({ gate: { ...this.args(), now: this.host.now(), revocations: this.host.revocations() } }); }
  private outbox(): CommitOutbox | null { return this.engine.query({ outbox: this.args() }); }
  private binding() { return { call: this.call, participation: this.participation }; }

  private async pass(): Promise<void> {
    await this.engine.run(async () => {
      this.assertOpen();
      if (!this.group) await this.enter();
      if (this.logDirty) { this.logDirty = false; await this.synchronize(); }
      await this.ownUpdate();
      await this.deliver();
    });
  }

  /** The parent must be this device's live channel group, as the server says. */
  private checkParent(): void {
    if (this.host.parentGroup(this.channel) !== this.channelGroup) throw new Error("voice parent is not the live channel group");
  }

  /** Create the singleton if the call has no group, else join the winner. */
  private async enter(): Promise<void> {
    this.checkParent();
    let group = this.initialGroup;
    if (group === null) {
      const d = await voiceDescriptor(this.host.identity, this.host.uid, this.host.server, this.channel, this.channelGroup, this.host.now());
      const info = await this.engine.op<number[]>({ create: { descriptor: d.bytes } });
      this.assertOpen();
      try { await this.host.request("mls.group.create", { conversation: `voice:${this.channel}`, group_info: encoded(info), voice: this.binding() }); }
      catch (error) { if (!(error instanceof RequestError) || error.code !== "conflict") throw error; }
      this.assertOpen();
      // Settle Creating against voice.state before exporting anything.
      const state = (await this.host.request("voice.state", {})).voice;
      this.assertOpen();
      if (!state || state.call !== this.call || state.participation !== this.participation || !state.group) throw new Error("voice call changed while creating its group");
      group = state.group;
      await this.engine.op({ settle: { server: this.host.server, group: [...decodeFixed(d.group, 32)], winner: bytes(group) } });
    }
    decodeFixed(group, 32);
    this.group = group;
    this.logDirty = true;
  }

  private async groupInfo(): Promise<{ info: number[]; tree: number[] }> {
    for (let attempt = 0; attempt < 3; attempt++) {
      const info = await this.host.request("mls.group_info", { group: this.group! });
      const tree = await this.host.tree(this.group!);
      this.assertOpen();
      if (tree.epoch === info.epoch) return { info: bytes(info.group_info), tree: tree.tree };
    }
    throw new Error("voice group changed while joining");
  }

  /** External commit only; a log gap or rejoin replaces this device's own leaf. */
  private async join(): Promise<void> {
    const { info, tree } = await this.groupInfo();
    await this.engine.op<CommitOutbox>({ join_external: { server: this.host.server, bytes: info, tree, now: this.host.now(), revocations: this.host.revocations() } }, out => {
      if (encoded(out.group) !== this.group) throw new Error("GroupInfo group mismatch");
      return [];
    });
  }

  private members(): GroupSummary["members"] { return this.summary()?.members ?? []; }

  /** A validated transition to `epoch`: hold first on any removal (including a
   * device's leaf replacement), then the barrier marks it processed. */
  private async transition(before: GroupSummary["members"], hash: string | null): Promise<void> {
    const summary = this.summary();
    if (summary?.record.status !== "active" || !summary.record.checkpoint) return;
    const epoch = BigInt(summary.record.checkpoint.epoch);
    if (this.processed !== null && epoch <= this.processed) return;
    const after = new Set(summary.members.map(m => `${m.leaf}/${m.device_key}`));
    const removed = before.filter(m => !after.has(`${m.leaf}/${m.device_key}`)).map(m => m.device_key);
    if (removed.length) await this.hold(removed);
    this.assertOpen();
    const now = this.host.now();
    this.pending = { started: this.pending?.started ?? now, removal: (this.pending?.removal ?? false) || removed.length > 0 };
    this.processed = epoch;
    this.processedDevices = new Set(summary.members.map(m => m.device_key));
    this.notify();
    this.acks.delete(epoch);
    for (const old of [...this.acks.keys()]) if (old < epoch) this.acks.delete(old);
    if (hash !== null && this.needUpdate && this.updateHash === hash && this.media) {
      // The accepted own update after a restart: installs may start here.
      this.media.floor = epoch; this.needUpdate = false; this.updateHash = null;
    }
    await this.barrier();
  }

  private async control(media: Media, body: Record<string, unknown>): Promise<ControlResult | null> {
    if (this.media !== media || this.closed) return null;
    media.serial++;
    const binding: VoiceKeyBinding = { server_key: this.host.server, session: this.host.session, channel: this.channel, call: this.call, participation: this.participation, run: media.run, group: this.group! };
    const result = await media.control({ binding, serial: String(media.serial), ...body } as Control);
    this.lastControl = { action: String(body.action), result };
    return this.media === media && !this.closed ? result : null;
  }

  /** Stop sending at once; removed devices' keys go before anything else. */
  private async hold(removed: string[]): Promise<void> {
    const media = this.media;
    if (!media) return;
    for (const device of removed) media.removed.add(device);
    if (media.activated !== null || removed.length) {
      media.activated = null;
      this.setStatus("securing");
      await this.control(media, { action: "hold", epoch: String(this.processed ?? 0n), removed });
    }
  }

  private async barrier(): Promise<void> {
    const media = this.media;
    if (!media || this.processed === null || (media.barrier !== null && media.barrier >= this.processed)) return;
    const epoch = this.processed;
    const result = await this.control(media, { action: "supersede", epoch: String(epoch) });
    if (result && "result" in result && result.result !== "ignored") media.barrier = epoch;
  }

  /** Follow the call group's log to its head, with its complete pending set. */
  private async synchronize(): Promise<void> {
    for (let attempt = 0, pages = 0; attempt < 8; attempt++) {
      this.assertOpen();
      if (++pages > 128) throw new Error("voice catch-up continues on the next pass");
      let local = this.summary();
      if (!local || local.record.status === "removed" || local.record.status === "rejoin") {
        await this.join(); local = this.summary()!;
      }
      const out = this.outbox();
      const since = wireEpoch(out?.epoch ?? local.record.checkpoint!.epoch);
      const log = await this.host.request("mls.log", { group: this.group!, since, limit: 100 });
      this.assertOpen();
      if (log.head < since) throw new Error("the server is behind this voice group");
      if (log.oldest > since) {
        if (out) await this.engine.op({ reconcile: { ...this.args(), result: { case: "pruned", oldest: String(log.oldest) } } });
        else await this.join();
        continue;
      }
      if (out) {
        const entry = log.entries.find(e => e.epoch === since);
        if (entry) {
          const before = this.members();
          let resolution: Resolution;
          try { resolution = await this.engine.op<Resolution>({ reconcile: { ...this.args(), result: { case: "entry", epoch: String(entry.epoch), commit: bytes(entry.commit), proposals: entry.proposals.map(bytes) } } }); }
          catch (error) { if (!(error instanceof AgentOperationError)) throw error; await this.join(); continue; }
          if (resolution === "accepted") {
            this.markJoined();
            await this.transition(before, encoded(out.hash));
          }
          continue;
        }
        if (log.head !== since) throw new Error("voice log omitted the pending commit epoch");
        await this.engine.op({ reconcile: { ...this.args(), result: { case: "head", epoch: String(log.head) } } });
        try {
          // Never merge a success reply by itself: the log entry is authoritative.
          await this.host.request("mls.commit", { group: this.group!, epoch: since, commit: encoded(out.commit), group_info: encoded(out.group_info), welcome: null, voice: this.binding() });
        } catch (error) {
          if (!(error instanceof RequestError)) throw error;
          const reason = error.code === "expired" ? "expired" : error.code === "early" ? "early" : error.code === "stale" ? "stale" : error.code === "bad_request" ? "bad_request" : "other";
          const resolution = await this.engine.op<Resolution>({ reconcile: { ...this.args(), result: { case: "rejected", reason } } });
          if (resolution === "retry") throw error;
        }
        continue;
      }
      let expected = since;
      for (const entry of log.entries) {
        if (entry.epoch !== expected) throw new Error("noncontiguous voice log");
        const before = this.members();
        try { await this.engine.op({ receive: { ...this.args(), commit: bytes(entry.commit), proposals: entry.proposals.map(bytes) } }); }
        catch (error) { if (!(error instanceof AgentOperationError)) throw error; await this.join(); attempt = -1; break; }
        expected++;
        await this.transition(before, null);
        if (this.summary()?.record.status !== "active") break;
      }
      if (attempt === -1 || this.summary()?.record.status !== "active") continue;
      if (expected < log.head || log.next !== null) { attempt--; continue; }
      if (expected !== log.head) throw new Error("voice log head mismatch");
      this.markJoined();
      if (this.processed === null) await this.transition([], null);
      // The complete pending set of this head, including an empty one.
      await this.engine.op({ voice_pending: { ...this.args(), epoch: String(log.head), proposals: log.pending.map(bytes) } });
      const gate = this.gate();
      if (gate.rejoin) { await this.join(); continue; }
      if (gate.server_pending || gate.blocked.length) {
        // A pending server removal blocks sending at once; commit it.
        await this.hold([]);
        await this.engine.op({ commit: { ...this.args(), adds: [], removes: [], now: this.host.now(), revocations: this.host.revocations() } });
        continue;
      }
      return;
    }
    throw new Error("voice group is still securing; retrying");
  }

  private markJoined(): void {
    if (this.summary()?.record.status === "active") { this.joinedResolve(this.group!); this.notify(); }
  }

  /** After a restart (or counter exhaustion/expiry) commit an own update; its
   * accepted epoch is the first one the new run may install. */
  private async ownUpdate(): Promise<void> {
    if (!this.needUpdate || !this.media || this.updateHash !== null || this.outbox()) return;
    const out = await this.engine.op<CommitOutbox>({ commit: { ...this.args(), adds: [], removes: [], now: this.host.now(), revocations: this.host.revocations() } });
    this.updateHash = encoded(out.hash);
    await this.synchronize();
  }

  /** Current parent membership gates sending; a missing leaf becomes a
   * removal only after the parent is synchronized with nothing outstanding. */
  private async parentRemovals(): Promise<boolean> {
    const synchronized = await this.host.syncParent(this.channelGroup);
    this.assertOpen();
    if (!synchronized) return false;
    const leaves = this.engine.withAgent(v => this.host.parent.withAgent(p => v.execute_with_parent!({ voice_parent_removals: { ...this.args(), synchronized: true } }, p))) as number[];
    if (!leaves.length) return false;
    await this.hold([]);
    await this.engine.op({ commit: { ...this.args(), adds: [], removes: leaves, now: this.host.now(), revocations: this.host.revocations() } });
    this.logDirty = true;
    await this.synchronize();
    return true;
  }

  /** The current epoch's keys, "blocked" when the gate refuses them, or
   * null while an agent reloads (a transient state, not a gate change). */
  private exportKeys(): VoiceKeys | "blocked" | null {
    try {
      return this.engine.withAgent(v => this.host.parent.withAgent(p => v.execute_with_parent!({ voice_keys: { ...this.args(), now: this.host.now(), revocations: this.host.revocations() } }, p))) as VoiceKeys;
    } catch (error) {
      const blocked = /group cannot send|invalid: voice parent/.test(String(error));
      if (String(error) !== this.lastExportError) { this.lastExportError = String(error); this.host.onError(error); }
      return blocked ? "blocked" : null;
    }
  }

  /** Barrier, installation, acknowledgement and activation for the newest processed epoch. */
  private async deliver(): Promise<void> {
    const media = this.media;
    if (!media || this.processed === null) return;
    await this.barrier();
    const epoch = this.processed;
    if (media.floor === null && this.needUpdate) return; // waiting for the restart's own update
    if (media.floor !== null && epoch < media.floor) return;
    if (media.installed !== epoch) {
      let keys = this.exportKeys();
      if (keys === "blocked") {
        // Blocked gate: stop sending, then resolve a missing parent member.
        this.gateBlocked = true;
        await this.hold([]);
        if (await this.parentRemovals()) return this.deliver();
        keys = this.exportKeys();
      }
      if (keys === "blocked" || keys === null) { this.schedule(true, keys === null ? 500 : 5_000); return; }
      if (BigInt(keys.epoch) !== epoch) { this.logDirty = true; this.dirty = true; return; }
      this.gateBlocked = false;
      const members = keys.members.map(m => ({ leaf: m.leaf, device: m.device, key: encodeBase64Url(new Uint8Array(m.key)) }));
      for (const m of keys.members) m.key.fill(0);
      const result = await this.control(media, { action: "install", epoch: keys.epoch, own_leaf: keys.own_leaf, members, removed: [...media.removed] });
      if (!result) return;
      if ("error" in result || result.result === "ignored") { this.logDirty = true; this.schedule(true, 1_000); return; }
      media.installed = epoch; media.removed.clear(); media.sendUntil = keys.send_until;
      const buffered = this.futureAcks.get(epoch);
      const acked = this.acks.get(epoch) ?? new Set<string>();
      for (const device of buffered?.devices ?? []) if (this.processedDevices.has(device)) acked.add(device);
      acked.add(this.host.identity.publicKey);
      this.acks.set(epoch, acked);
      for (const old of [...this.futureAcks.keys()]) if (old <= epoch) this.futureAcks.delete(old);
      await this.sendAck(media, epoch);
      if (this.lostAcks && this.host.now() - this.lastRepeat >= REPEAT_EVERY_MS) {
        this.lostAcks = false; this.lastRepeat = this.host.now();
        await this.request("voice.epoch.repeat", { ...this.binding(), run: media.run, group: this.group!, epoch: wireEpoch(epoch) });
      }
    } else {
      const keys = this.exportKeys();
      if (keys === null) { this.schedule(false, 500); return; }
      if (this.gateBlocked || keys === "blocked") {
        // Revocation, expiry or a parent change since installation closes the gate.
        this.gateBlocked = true;
        await this.hold([]);
        if (!(await this.parentRemovals())) this.schedule(true, 5_000);
        const again = this.exportKeys();
        if (again === null || again === "blocked") return;
        this.gateBlocked = false;
        media.sendUntil = again.send_until;
      } else media.sendUntil = keys.send_until;
    }
    await this.activate(media, epoch);
  }

  private async activate(media: Media, epoch: bigint): Promise<void> {
    if (media.activated === epoch || media.installed !== epoch || !this.pending && media.activated !== null) return;
    const acked = this.acks.get(epoch) ?? new Set();
    const everyone = [...this.processedDevices].every(device => acked.has(device));
    const started = this.pending?.started ?? this.host.now();
    const due = started + ACK_WAIT_MS;
    if (!(this.pending?.removal || everyone || this.host.now() >= due)) {
      // Recheck elapsed time on every wake; suspension cannot restart the wait.
      clearTimeout(this.activation);
      this.activation = setTimeout(() => this.schedule(false), Math.max(0, due - this.host.now()));
      return;
    }
    const result = await this.control(media, { action: "activate", epoch: String(epoch), send_until: media.sendUntil });
    if (!result) return;
    if ("result" in result && result.result !== "ignored") {
      media.activated = epoch; this.pending = null;
      this.setStatus("active");
    } else this.schedule(false, 500);
  }

  private async sendAck(media: Media, epoch: bigint): Promise<void> {
    await this.request("voice.epoch.ack", { ...this.binding(), run: media.run, group: this.group!, epoch: wireEpoch(epoch) });
  }

  private async request<O extends Op>(op: O, data: RequestInput<O>): Promise<void> {
    try { await this.host.request(op, data); }
    catch (error) { if (!(error instanceof RequestError)) throw error; this.host.onError(error); }
  }
}
