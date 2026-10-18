/**
 * The SFrame key state of one encrypted voice run (docs/e2e-build.md
 * "SFrame and receiver state" and "Installation and activation"). The worker
 * owns one instance; the native engine implements the same control contract.
 * Key material never leaves this object: no logs, persistence or archives.
 */
import { sha256 } from "@noble/hashes/sha2.js";
import { decodeBase64Url, encodeBase64Url } from "../../net/base64url";
import { decodeHeader } from "./header";
import { packetSamples } from "./opus";
import { Replay } from "./replay";
import { deriveKey, open, seal, TAG, type SuiteKey } from "./suite";

export interface VoiceKeyBinding {
  server_key: string;
  session: number;
  channel: number;
  call: string;
  participation: string;
  run: number;
  group: string;
}
interface Common { binding: VoiceKeyBinding; serial: string; epoch: string }
export type Control =
  | (Common & { action: "hold"; removed: string[] })
  | (Common & { action: "supersede" })
  | (Common & { action: "install"; own_leaf: number; members: { leaf: number; device: string; key: string }[]; removed: string[] })
  | (Common & { action: "activate"; send_until: number | null });
export type ControlError = "bad_keys" | "not_installed" | "key_conflict" | "key_queue_full";
export type ControlResult = { serial: string; epoch: string; result: "applied" | "unchanged" | "ignored" } | { serial: string; epoch: string; error: ControlError };
export type StatusReason = "counter_exhausted" | "key_expired" | "key_conflict" | "key_queue_full" | "worker_failed";
export interface CryptoStatus { binding: VoiceKeyBinding; state: "securing" | "failed"; reason: StatusReason }
export const COUNTERS = ["encrypted", "decrypted", "no_key", "auth_failed", "replay_dropped", "own_kid_dropped", "oversize_dropped", "unexpected_duration", "stale_install", "counter_exhausted"] as const;
export type Counters = Record<(typeof COUNTERS)[number], number>;

const U64_MAX = (1n << 64n) - 1n;
/** Encryption attempts under one base key before an own-leaf update is required. */
export const COUNTER_LIMIT = 1n << 32n;
export const MAX_EPOCHS = 8;
export const RETAIN_MS = 10_000;
export const MAX_CONTROL_BYTES = 256 * 1024;
const MAX_LEAVES = 1000;
const QUEUE = 8;
const MIN_FRAME = 1 + TAG, MAX_FRAME = 1033, MAX_OPUS = 1000;

export const voiceKid = (epoch: bigint, leaf: number): bigint => ((epoch & 0xffffffffn) << 32n) | BigInt(leaf);
const decimal = (value: unknown): bigint | null => typeof value === "string" && /^(0|[1-9][0-9]{0,19})$/.test(value) && BigInt(value) <= U64_MAX ? BigInt(value) : null;
const u32 = (value: unknown): value is number => Number.isInteger(value) && (value as number) >= 0 && (value as number) <= 0xffffffff;
const safe = (value: unknown): value is number => Number.isSafeInteger(value) && (value as number) >= 0;
function fixed(value: unknown, length: number): boolean {
  if (typeof value !== "string") return false;
  try { const bytes = decodeBase64Url(value); return bytes.length === length && encodeBase64Url(bytes) === value; } catch { return false; }
}
const exact = (value: object, keys: string[]) => Object.keys(value).length === keys.length && keys.every(k => k in value);

function bindingOf(value: unknown): VoiceKeyBinding | null {
  if (typeof value !== "object" || value === null) return null;
  const b = value as Record<string, unknown>;
  if (!exact(b, ["server_key", "session", "channel", "call", "participation", "run", "group"])) return null;
  if (!fixed(b.server_key, 32) || !safe(b.session) || !u32(b.channel) || !fixed(b.call, 16) || !fixed(b.participation, 16) || !safe(b.run) || b.run === 0 || !fixed(b.group, 32)) return null;
  return { server_key: b.server_key as string, session: b.session as number, channel: b.channel as number, call: b.call as string, participation: b.participation as string, run: b.run as number, group: b.group as string };
}
const bindingKey = (b: VoiceKeyBinding) => JSON.stringify([b.server_key, b.session, b.channel, b.call, b.participation, b.run, b.group]);

function devices(value: unknown): string[] | null {
  if (!Array.isArray(value) || value.length > MAX_LEAVES || !value.every(d => fixed(d, 32)) || new Set(value).size !== value.length) return null;
  return value as string[];
}

/** Validates the exact tagged control body; null for any structural error. */
export function parseControl(value: unknown): Control | null {
  if (typeof value !== "object" || value === null) return null;
  const c = value as Record<string, unknown>;
  const binding = bindingOf(c.binding);
  const serial = decimal(c.serial);
  if (!binding || serial === null || serial === 0n || decimal(c.epoch) === null) return null;
  const common = { binding, serial: c.serial as string, epoch: c.epoch as string };
  switch (c.action) {
    case "hold": {
      const removed = devices(c.removed);
      return exact(c, ["action", "binding", "serial", "epoch", "removed"]) && removed ? { ...common, action: "hold", removed } : null;
    }
    case "supersede": return exact(c, ["action", "binding", "serial", "epoch"]) ? { ...common, action: "supersede" } : null;
    case "activate": {
      const until = c.send_until;
      return exact(c, ["action", "binding", "serial", "epoch", "send_until"]) && (until === null || safe(until)) ? { ...common, action: "activate", send_until: until as number | null } : null;
    }
    case "install": {
      const removed = devices(c.removed);
      if (!exact(c, ["action", "binding", "serial", "epoch", "own_leaf", "members", "removed"]) || !removed || !u32(c.own_leaf) || !Array.isArray(c.members)) return null;
      const members = c.members as unknown[];
      if (members.length === 0 || members.length > MAX_LEAVES) return null;
      const seen = new Set<string>();
      let previous = -1, own = 0;
      for (const m of members) {
        if (typeof m !== "object" || m === null || !exact(m, ["leaf", "device", "key"])) return null;
        const { leaf, device, key } = m as Record<string, unknown>;
        if (!u32(leaf) || leaf <= previous || !fixed(device, 32) || !fixed(key, 32) || seen.has(device as string)) return null;
        previous = leaf; seen.add(device as string);
        if (leaf === c.own_leaf) own++;
      }
      if (own !== 1) return null;
      return { ...common, action: "install", own_leaf: c.own_leaf, members: members as { leaf: number; device: string; key: string }[], removed };
    }
    default: return null;
  }
}

/** A non-secret fingerprint of a key-bearing text; the text itself is not kept. */
const fingerprint = (text: string): string => encodeBase64Url(sha256(new TextEncoder().encode(text)));

/** Fixed field order: bytewise comparison of duplicate serials. */
export function canonical(c: Control): string {
  const b = c.binding;
  const binding = { server_key: b.server_key, session: b.session, channel: b.channel, call: b.call, participation: b.participation, run: b.run, group: b.group };
  switch (c.action) {
    case "hold": return JSON.stringify({ action: c.action, binding, serial: c.serial, epoch: c.epoch, removed: c.removed });
    case "supersede": return JSON.stringify({ action: c.action, binding, serial: c.serial, epoch: c.epoch });
    case "activate": return JSON.stringify({ action: c.action, binding, serial: c.serial, epoch: c.epoch, send_until: c.send_until });
    case "install": return JSON.stringify({ action: c.action, binding, serial: c.serial, epoch: c.epoch, own_leaf: c.own_leaf, members: c.members.map(m => ({ leaf: m.leaf, device: m.device, key: m.key })), removed: c.removed });
  }
}

interface Receiver { device: string; key: SuiteKey; replay: Replay }
interface EpochKeys {
  epoch: bigint;
  supersededAt: number | null;
  ownLeaf: number;
  own: SuiteKey;
  /** Reserved before each attempt; never reused under this base key. */
  counter: bigint;
  /** Monotonic send deadline; activation can only shorten it. */
  deadline: number;
  /** The absolute expiry (Unix ms), also checked: the monotonic clock may stop while the machine sleeps. */
  expiresAt: number;
  receivers: Map<number, Receiver>;
  digest: string;
}

export interface StateOptions {
  binding: VoiceKeyBinding;
  /** The controller's highest processed epoch before a restart's own update. */
  resumeAfter: bigint | null;
  /** Monotonic milliseconds. */
  now: () => number;
  /** Unix milliseconds, only to convert `send_until`. */
  wallNow: () => number;
  onStatus: (status: CryptoStatus) => void;
  subtle?: SubtleCrypto;
  /** This client's configured packet duration in 48 kHz samples (browser: 20 ms). */
  frameSamples?: number;
}

export class SframeState {
  readonly counters: Counters = Object.fromEntries(COUNTERS.map(c => [c, 0])) as Counters;
  private readonly binding: string;
  private ended: StatusReason | "stopped" | null = null;
  private serial = 0n;
  private last: { body: string; done: Promise<ControlResult> } | null = null;
  private pending = 0;
  /** The processed-epoch floor: barriers advance it, installs require it. */
  private floor: bigint | null = null;
  private readonly epochs = new Map<bigint, EpochKeys>();
  private latest: { epoch: bigint; digest: string; devices: Set<string> } | null = null;
  private readonly importing = new Map<bigint, { digest: string; done: Promise<ControlResult> }>();
  private readonly lastAuthenticated = new Map<string, bigint>();
  private readonly removals = new Map<string, number>();
  private removalGeneration = 0;
  private sender: EpochKeys | null = null;
  private sendGeneration = 0;
  private securing: StatusReason | null = null;

  constructor(private readonly options: StateOptions) {
    this.binding = bindingKey(options.binding);
  }

  get stopped(): boolean { return this.ended !== null; }
  get sending(): boolean { return this.sender !== null; }
  /** Retained full epochs, oldest first (diagnostics and tests). */
  get retained(): bigint[] { return [...this.epochs.keys()].sort((a, b) => (a < b ? -1 : 1)); }

  /** Applies one control in arrival order. Installation imports keys
   * asynchronously; later barriers are applied without waiting for it. */
  control(body: unknown, bytes = 0): Promise<ControlResult> {
    const c = bytes <= MAX_CONTROL_BYTES ? parseControl(body) : null;
    const tag = { serial: typeof (body as Control)?.serial === "string" ? (body as Control).serial : "0", epoch: typeof (body as Control)?.epoch === "string" ? (body as Control).epoch : "0" };
    if (!c) return Promise.resolve({ ...tag, error: "bad_keys" });
    const ignored = (): ControlResult => ({ serial: c.serial, epoch: c.epoch, result: "ignored" });
    if (this.ended || bindingKey(c.binding) !== this.binding) return Promise.resolve(ignored());
    const serial = BigInt(c.serial), text = fingerprint(canonical(c));
    if (serial < this.serial) return Promise.resolve(ignored());
    if (serial === this.serial && this.last) {
      if (this.last.body !== text) return Promise.resolve(this.fail("key_conflict", c));
      return this.last.done.then(r => ("result" in r && r.result === "applied" ? { ...r, result: "unchanged" } : r));
    }
    if (this.pending >= QUEUE) return Promise.resolve(this.fail("key_queue_full", c));
    this.serial = serial;
    this.pending++;
    const done = this.apply(c).finally(() => { this.pending--; });
    this.last = { body: text, done };
    return done;
  }

  private fail(reason: "key_conflict" | "key_queue_full", c: Control): ControlResult {
    this.stop(reason);
    return { serial: c.serial, epoch: c.epoch, error: reason };
  }

  /** An unexpected failure (a rejected import, a WebCrypto error) closes the run. */
  private async apply(c: Control): Promise<ControlResult> {
    try { return await this.applyControl(c); }
    catch {
      this.stop("worker_failed");
      return { serial: c.serial, epoch: c.epoch, result: "ignored" };
    }
  }

  private async applyControl(c: Control): Promise<ControlResult> {
    const epoch = BigInt(c.epoch);
    const result = (result: "applied" | "unchanged" | "ignored"): ControlResult => ({ serial: c.serial, epoch: c.epoch, result });
    switch (c.action) {
      case "hold":
        this.stopSending();
        this.purge(c.removed);
        return result("applied");
      case "supersede": {
        if (this.options.resumeAfter !== null && epoch <= this.options.resumeAfter) return result("ignored");
        if (this.floor !== null && epoch <= this.floor) return result(epoch === this.floor ? "unchanged" : "ignored");
        const now = this.options.now();
        // E's retention starts at its first supersession; retries keep it.
        for (const keys of this.epochs.values()) if (keys.epoch < epoch && keys.supersededAt === null) keys.supersededAt = now;
        this.floor = epoch;
        return result("applied");
      }
      case "activate": {
        if (this.floor !== null && epoch < this.floor) return result("ignored");
        const keys = this.epochs.get(epoch);
        if (!keys || epoch !== this.floor) return { serial: c.serial, epoch: c.epoch, error: "not_installed" };
        if (this.sender && this.sender.epoch > epoch) return result("ignored");
        const deadline = c.send_until === null ? Infinity : this.options.now() + (c.send_until - this.options.wallNow());
        keys.deadline = Math.min(keys.deadline, deadline);
        if (c.send_until !== null) keys.expiresAt = Math.min(keys.expiresAt, c.send_until);
        const unchanged = this.sender === keys;
        this.sender = keys;
        this.securing = null;
        return result(unchanged ? "unchanged" : "applied");
      }
      case "install": return this.install(c, epoch, result);
    }
  }

  private async install(c: Extract<Control, { action: "install" }>, epoch: bigint, result: (r: "applied" | "unchanged" | "ignored") => ControlResult): Promise<ControlResult> {
    const stale = () => { this.counters.stale_install++; return result("ignored"); };
    if (this.options.resumeAfter !== null && epoch <= this.options.resumeAfter) return stale();
    if (this.floor === null || epoch > this.floor) return { serial: c.serial, epoch: c.epoch, error: "bad_keys" };
    if (epoch < this.floor) return stale();
    // Normalized member fields, fingerprinted: no raw exporter key is retained.
    const digest = fingerprint(JSON.stringify([c.own_leaf, c.members.map(m => [m.leaf, m.device, m.key])]));
    const same = (other: string) => {
      if (other === digest) return result("unchanged");
      this.stop("key_conflict");
      return { serial: c.serial, epoch: c.epoch, error: "key_conflict" } as ControlResult;
    };
    if (this.latest?.epoch === epoch) return same(this.latest.digest);
    if (this.latest && epoch < this.latest.epoch) return stale();
    const inFlight = this.importing.get(epoch);
    if (inFlight) {
      const first = await inFlight.done;
      return "error" in first ? first : first.result === "ignored" ? stale() : same(inFlight.digest);
    }
    const done = this.importKeys(c, epoch, digest, result, stale);
    this.importing.set(epoch, { digest, done });
    try { return await done; } finally { this.importing.delete(epoch); }
  }

  private async importKeys(c: Extract<Control, { action: "install" }>, epoch: bigint, digest: string, result: (r: "applied" | "unchanged" | "ignored") => ControlResult, stale: () => ControlResult): Promise<ControlResult> {
    // Removed devices go first: a reused leaf index must not inherit them.
    this.purge(c.removed);
    const fences = new Map(c.members.map(m => [m.device, this.removals.get(m.device) ?? 0]));
    const subtle = this.options.subtle ?? crypto.subtle;
    const keys = await Promise.all(c.members.map(async m => {
      const base = new Uint8Array(decodeBase64Url(m.key));
      try { return await deriveKey(voiceKid(epoch, m.leaf), base, m.leaf === c.own_leaf ? "encrypt" : "decrypt", subtle); }
      finally { base.fill(0); }
    }));
    // A superseded or stopped import publishes nothing and is never acknowledged.
    if (this.ended) return result("ignored");
    if (this.floor !== epoch || (this.latest && this.latest.epoch >= epoch)) return stale();
    const receivers = new Map<number, Receiver>();
    let own: SuiteKey | null = null;
    c.members.forEach((m, i) => {
      if (m.leaf === c.own_leaf) own = keys[i]!;
      // A device removed while this import ran stays purged.
      else if ((this.removals.get(m.device) ?? 0) === fences.get(m.device)) receivers.set(m.leaf, { device: m.device, key: keys[i]!, replay: new Replay() });
    });
    this.epochs.set(epoch, { epoch, supersededAt: null, ownLeaf: c.own_leaf, own: own!, counter: 0n, deadline: Infinity, expiresAt: Infinity, receivers, digest });
    this.latest = { epoch, digest, devices: new Set(c.members.filter(m => m.leaf !== c.own_leaf).map(m => m.device)) };
    for (const device of this.lastAuthenticated.keys()) if (!this.latest.devices.has(device)) this.lastAuthenticated.delete(device);
    this.sweep();
    return result("applied");
  }

  /** Every removal purges the device from all kept epochs and discards its
   * in-flight results; only a new validated epoch can give it keys again. */
  private purge(removed: string[]): void {
    for (const device of removed) {
      for (const keys of this.epochs.values()) for (const [leaf, r] of keys.receivers) if (r.device === device) keys.receivers.delete(leaf);
      this.lastAuthenticated.delete(device);
      this.removals.set(device, ++this.removalGeneration);
    }
  }

  private stopSending(): void {
    this.sender = null;
    this.sendGeneration++;
  }

  /** Retention: the processed head, 10 s from first supersession, or a
   * current device's latest authenticated epoch; never more than eight. */
  sweep(): void {
    if (this.ended) return;
    const now = this.options.now();
    const authenticated = new Set(this.lastAuthenticated.values());
    for (const keys of [...this.epochs.values()]) {
      const keep = keys.epoch === this.floor || keys.supersededAt === null || now - keys.supersededAt < RETAIN_MS || authenticated.has(keys.epoch);
      if (!keep) this.evict(keys);
    }
    for (const epoch of this.retained.slice(0, Math.max(0, this.epochs.size - MAX_EPOCHS))) this.evict(this.epochs.get(epoch)!);
  }

  private evict(keys: EpochKeys): void {
    this.epochs.delete(keys.epoch);
    if (this.sender === keys) this.stopSending();
  }

  private status(state: "securing" | "failed", reason: StatusReason): void {
    this.options.onStatus({ binding: this.options.binding, state, reason });
  }

  private closeSending(reason: "counter_exhausted" | "key_expired"): void {
    this.stopSending();
    if (this.securing === reason) return;
    this.securing = reason;
    this.status("securing", reason);
  }

  /** Stops the run: no later control, frame or result passes. */
  stop(reason: StatusReason | "stopped" = "stopped"): void {
    if (this.ended) return;
    this.ended = reason;
    this.stopSending();
    this.epochs.clear();
    this.lastAuthenticated.clear();
    this.latest = null;
    this.last = null;
    if (reason !== "stopped") this.status("failed", reason);
  }

  /** One Opus packet in, one SFrame ciphertext out, or null to drop it. */
  async encrypt(plaintext: Uint8Array<ArrayBuffer>): Promise<Uint8Array<ArrayBuffer> | null> {
    const keys = this.sender;
    if (this.ended || !keys) return null;
    if (plaintext.length < 1 || plaintext.length > MAX_OPUS) { this.counters.oversize_dropped++; return null; }
    // Duration is parsed before encryption: malformed Opus never leaves; a
    // valid packet of another duration is counted but sent.
    const samples = packetSamples(plaintext);
    if (samples === null) { this.counters.unexpected_duration++; return null; }
    if (samples !== (this.options.frameSamples ?? 960)) this.counters.unexpected_duration++;
    const expired = () => this.options.now() >= keys.deadline || this.options.wallNow() >= keys.expiresAt;
    if (expired()) { this.closeSending("key_expired"); return null; }
    if (keys.counter >= COUNTER_LIMIT) { this.counters.counter_exhausted++; this.closeSending("counter_exhausted"); return null; }
    const ctr = keys.counter++;
    const generation = this.sendGeneration;
    const ciphertext = await seal(keys.own, voiceKid(keys.epoch, keys.ownLeaf), ctr, new Uint8Array(), plaintext, this.options.subtle);
    if (this.ended || generation !== this.sendGeneration || this.sender !== keys) return null;
    // A completion after the certificate deadline is dropped too.
    if (expired()) { this.closeSending("key_expired"); return null; }
    this.counters.encrypted++;
    return ciphertext;
  }

  /** One SFrame ciphertext in, a validated Opus packet out, or null. */
  async decrypt(frame: Uint8Array): Promise<Uint8Array<ArrayBuffer> | null> {
    if (this.ended) return null;
    if (frame.length < MIN_FRAME || frame.length > MAX_FRAME) { this.counters.oversize_dropped++; return null; }
    const header = decodeHeader(frame);
    if (!header) { this.counters.auth_failed++; return null; }
    // Retention is exact: an epoch past its window opens nothing, whatever the sweep timer.
    this.sweep();
    const low = header.kid >> 32n, leaf = Number(header.kid & 0xffffffffn);
    const newest = [...this.epochs.values()].filter(k => (k.epoch & 0xffffffffn) === low).sort((a, b) => (a.epoch > b.epoch ? -1 : 1));
    if (newest.some(k => k.ownLeaf === leaf)) { this.counters.own_kid_dropped++; return null; }
    const candidates = newest.flatMap(k => { const r = k.receivers.get(leaf); return r ? [{ keys: k, receiver: r }] : []; });
    if (!candidates.length) { this.counters.no_key++; return null; }
    const aad = frame.slice(0, header.length), body = frame.slice(header.length);
    let replayed = false;
    for (const { keys, receiver } of candidates) {
      if (!receiver.replay.fresh(header.ctr)) { replayed = true; continue; }
      const fence = this.removals.get(receiver.device) ?? 0;
      const plaintext = await open(receiver.key, aad, header.ctr, new Uint8Array(), body, this.options.subtle);
      if (!plaintext) continue;
      // Recheck after the await: the run, the retained key and its device's membership.
      if (this.ended || this.epochs.get(keys.epoch) !== keys || keys.receivers.get(leaf) !== receiver || (this.removals.get(receiver.device) ?? 0) !== fence) {
        this.counters.no_key++; return null;
      }
      if (!receiver.replay.admit(header.ctr)) { this.counters.replay_dropped++; return null; }
      if (plaintext.length > MAX_OPUS || packetSamples(plaintext) === null) { this.counters.unexpected_duration++; return null; }
      if (this.latest?.devices.has(receiver.device)) {
        const previous = this.lastAuthenticated.get(receiver.device);
        if (previous === undefined || keys.epoch > previous) this.lastAuthenticated.set(receiver.device, keys.epoch);
      }
      this.counters.decrypted++;
      this.sweep();
      return plaintext;
    }
    if (replayed) this.counters.replay_dropped++;
    else this.counters.auth_failed++;
    return null;
  }
}
