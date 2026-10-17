import { describe, expect, it } from "vitest";
import { encodeBase64Url } from "../../net/base64url";
import { decodeHeader, encodeHeader } from "./header";
import { COUNTER_LIMIT, parseControl, SframeState, voiceKid, type Control, type CryptoStatus, type VoiceKeyBinding } from "./state";
import { deriveKey, seal } from "./suite";

const b64 = (bytes: Uint8Array) => encodeBase64Url(bytes);
const filled = (n: number, v: number) => b64(new Uint8Array(n).fill(v));
const binding: VoiceKeyBinding = { server_key: filled(32, 1), session: 7, channel: 3, call: filled(16, 2), participation: filled(16, 3), run: 1, group: filled(32, 4) };
const DEVICES = [filled(32, 10), filled(32, 11), filled(32, 12), filled(32, 13)];
const OPUS = new Uint8Array([0xf8, 1, 2, 3]); // 20 ms CELT, one frame

/** The exporter output every member computes for (epoch, leaf). */
function exporter(epoch: bigint, leaf: number, salt = 0): string {
  const key = new Uint8Array(32);
  new DataView(key.buffer).setBigUint64(0, epoch);
  new DataView(key.buffer).setUint32(8, leaf);
  key[12] = salt;
  return b64(key);
}
type Member = { leaf: number; device: string; key: string };
const members = (epoch: bigint, leaves: [number, number][], salt = 0): Member[] =>
  leaves.map(([leaf, device]) => ({ leaf, device: DEVICES[device]!, key: exporter(epoch, leaf, salt) }));

class Clock { t = 1_000; wall = 1_800_000_000_000; now = () => this.t; wallNow = () => this.wall; }

function engine(clock = new Clock(), resumeAfter: bigint | null = null) {
  const statuses: CryptoStatus[] = [];
  const state = new SframeState({ binding, resumeAfter, now: clock.now, wallNow: clock.wallNow, onStatus: s => statuses.push(s) });
  let serial = 0;
  const send = (c: Record<string, unknown>) => state.control({ binding, serial: String(++serial), ...c });
  const ok = async (c: Record<string, unknown>) => {
    const r = await send(c);
    expect(r, JSON.stringify(c.action)).toMatchObject({ result: "applied" });
    return r;
  };
  /** Barrier, install and activation of a processed epoch. */
  const enter = async (epoch: bigint, own: number, all: Member[], removed: string[] = []) => {
    await ok({ action: "supersede", epoch: String(epoch) });
    await ok({ action: "install", epoch: String(epoch), own_leaf: own, members: all, removed });
    await ok({ action: "activate", epoch: String(epoch), send_until: null });
  };
  return { state, statuses, send, ok, enter, clock, nextSerial: () => String(++serial) };
}

describe("SframeState", () => {
  it("round-trips between two devices with one-directional keys", async () => {
    const a = engine(), b = engine();
    const all = members(5n, [[0, 0], [1, 1]]);
    await a.enter(5n, 0, all);
    await b.enter(5n, 1, all);
    const frame = (await a.state.encrypt(OPUS.slice()))!;
    expect(decodeHeader(frame)).toMatchObject({ kid: voiceKid(5n, 0), ctr: 0n });
    expect(await b.state.decrypt(frame)).toEqual(OPUS);
    // A device never decrypts its own kid, before trying any key.
    expect(await a.state.decrypt(frame)).toBeNull();
    expect(a.state.counters.own_kid_dropped).toBe(1);
    expect(a.state.counters.auth_failed + a.state.counters.no_key).toBe(0);
    expect(b.state.counters.decrypted).toBe(1);
  });

  it("starts closed and drops without keys", async () => {
    const a = engine();
    expect(await a.state.encrypt(OPUS.slice())).toBeNull();
    const enc = await deriveKey(voiceKid(1n, 1), new Uint8Array(32), "encrypt");
    expect(await a.state.decrypt(await seal(enc, voiceKid(1n, 1), 0n, new Uint8Array(), OPUS.slice()))).toBeNull();
    expect(a.state.counters.no_key).toBe(1);
  });

  it("admits each counter once across slots and concurrent decrypts", async () => {
    const a = engine(), b = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await a.enter(1n, 0, all); await b.enter(1n, 1, all);
    const frame = (await a.state.encrypt(OPUS.slice()))!;
    const results = await Promise.all([b.state.decrypt(frame), b.state.decrypt(frame), b.state.decrypt(frame)]);
    expect(results.filter(Boolean).length).toBe(1);
    expect(b.state.counters.replay_dropped).toBe(2);
  });

  it("never mutates receive state on failed authentication", async () => {
    const a = engine(), b = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await a.enter(1n, 0, all); await b.enter(1n, 1, all);
    const frame = (await a.state.encrypt(OPUS.slice()))!;
    for (let i = 0; i < frame.length; i++) {
      const bad = frame.slice(); bad[i]! ^= 0x40;
      expect(await b.state.decrypt(bad), `byte ${i}`).toBeNull();
    }
    // Plaintext injection and truncation are rejected before cryptography.
    expect(await b.state.decrypt(OPUS)).toBeNull();
    expect(await b.state.decrypt(frame.subarray(0, 16))).toBeNull();
    expect(b.state.counters.decrypted).toBe(0);
    expect(await b.state.decrypt(frame)).toEqual(OPUS);
  });

  it("drops frames outside 17-1033 bytes and invalid opened Opus", async () => {
    const a = engine(), b = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await a.enter(1n, 0, all); await b.enter(1n, 1, all);
    expect(await a.state.encrypt(new Uint8Array(1001))).toBeNull();
    expect(a.state.counters.oversize_dropped).toBe(1);
    expect(await b.state.decrypt(new Uint8Array(1034))).toBeNull();
    expect(b.state.counters.oversize_dropped).toBe(1);
    // The sender drops malformed Opus before encryption and counts other durations.
    expect(await a.state.encrypt(new Uint8Array([0xf1, 1]))).toBeNull(); // CBR two frames, odd body
    expect(a.state.counters.unexpected_duration).toBe(1);
    expect(await a.state.encrypt(new Uint8Array([0xf0, 1]))).not.toBeNull(); // valid 10 ms: counted, sent
    expect(a.state.counters.unexpected_duration).toBe(2);
    const widest = (await a.state.encrypt(new Uint8Array([0xf8, ...new Array<number>(999).fill(7)])))!;
    expect(widest.length).toBeLessThanOrEqual(1033);
    expect((await b.state.decrypt(widest))?.length).toBe(1000);
    // A malformed packet from another sender's key is dropped after opening.
    const enc = await deriveKey(voiceKid(1n, 0), new Uint8Array(Buffer.from(all[0]!.key, "base64url")), "encrypt");
    const invalid = await seal(enc, voiceKid(1n, 0), 1000n, new Uint8Array(), new Uint8Array([0xf1, 1]));
    expect(await b.state.decrypt(invalid)).toBeNull();
    expect(b.state.counters.unexpected_duration).toBe(1);
  });

  it("follows the 128-counter window and the widest header encodings", async () => {
    const b = engine();
    const all = members(0xffffffffn, [[0, 0], [0xffffffff, 1]]);
    await b.enter(0xffffffffn, 0, all);
    const enc = await deriveKey(voiceKid(0xffffffffn, 0xffffffff), new Uint8Array(Buffer.from(all[1]!.key, "base64url")), "encrypt");
    const frame = (ctr: bigint) => seal(enc, voiceKid(0xffffffffn, 0xffffffff), ctr, new Uint8Array(), OPUS.slice());
    const high = (1n << 64n) - 1n;
    expect((await frame(high)).length).toBe(17 + OPUS.length + 16);
    expect(await b.state.decrypt(await frame(high))).toEqual(OPUS);
    expect(await b.state.decrypt(await frame(high - 127n))).toEqual(OPUS);
    expect(await b.state.decrypt(await frame(high - 128n))).toBeNull();
    expect(b.state.counters.replay_dropped).toBe(1);
  });

  it("separates epochs whose low 32 bits collide", async () => {
    const a = engine(), b = engine();
    const low = 7n, high = 7n + (1n << 32n);
    await a.enter(low, 0, members(low, [[0, 0], [1, 1]]));
    await b.enter(low, 1, members(low, [[0, 0], [1, 1]]));
    const old = (await a.state.encrypt(OPUS.slice()))!;
    await a.enter(high, 0, members(high, [[0, 0], [1, 1]]));
    await b.enter(high, 1, members(high, [[0, 0], [1, 1]]));
    const fresh = (await a.state.encrypt(OPUS.slice()))!;
    expect(decodeHeader(old)!.kid).toBe(decodeHeader(fresh)!.kid);
    expect(await b.state.decrypt(fresh)).toEqual(OPUS);
    expect(await b.state.decrypt(old)).toEqual(OPUS); // the older epoch's key, newest first
    expect(b.state.retained).toEqual([low, high]);
  });

  it("purges a removed device everywhere before its leaf index is reused", async () => {
    const a = engine(), b = engine(), c = engine();
    await a.enter(1n, 0, members(1n, [[0, 0], [1, 1]]));
    await b.enter(1n, 1, members(1n, [[0, 0], [1, 1]]));
    const before = (await a.state.encrypt(OPUS.slice()))!;
    // Device 0 is removed; device 2 takes leaf 0 in epoch 2.
    await b.ok({ action: "hold", epoch: "1", removed: [DEVICES[0]!] });
    expect(await b.state.decrypt(before)).toBeNull();
    await b.enter(2n, 1, members(2n, [[0, 2], [1, 1]]), [DEVICES[0]!]);
    await c.enter(2n, 0, members(2n, [[0, 2], [1, 1]]));
    expect(await b.state.decrypt(before)).toBeNull();
    expect(await b.state.decrypt((await c.state.encrypt(OPUS.slice()))!)).toEqual(OPUS);
  });

  it("retains a silent sender's epoch, and expires others 10 s after the first barrier", async () => {
    const clock = new Clock();
    const a = engine(), b = engine(clock), c = engine();
    const one = members(1n, [[0, 0], [1, 1], [2, 2]]);
    await a.enter(1n, 0, one); await b.enter(1n, 1, one); await c.enter(1n, 2, one);
    expect(await b.state.decrypt((await a.state.encrypt(OPUS.slice()))!)).toEqual(OPUS);
    // Device 2 never spoke at epoch 1; device 0 did and then stays silent.
    const two = members(2n, [[0, 0], [1, 1], [2, 2]]);
    await b.enter(2n, 1, two); await c.enter(2n, 2, two);
    clock.t += 10_000; b.state.sweep();
    expect(b.state.retained).toEqual([1n, 2n]);
    expect(await b.state.decrypt((await a.state.encrypt(OPUS.slice()))!)).toEqual(OPUS);
    await a.enter(2n, 0, two);
    expect(await b.state.decrypt((await a.state.encrypt(OPUS.slice()))!)).toEqual(OPUS);
    expect(b.state.retained).toEqual([2n]);
    // A late older frame cannot move the pointer back.
    const late = engine();
    await late.enter(1n, 0, one);
    expect(await b.state.decrypt((await late.state.encrypt(OPUS.slice()))!)).toBeNull();
  });

  it("starts retention at the barrier, not the import (delayed barrier and delayed import)", async () => {
    // Barrier for epoch 2 applied at engine time 1 s, prompt install: epoch 1 kept until 11 s.
    const clock = new Clock(); clock.t = 0;
    const b = engine(clock);
    await b.enter(1n, 1, members(1n, [[0, 0], [1, 1]]));
    clock.t = 1_000;
    await b.ok({ action: "supersede", epoch: "2" });
    await b.ok({ action: "install", epoch: "2", own_leaf: 1, members: members(2n, [[0, 0], [1, 1]]), removed: [] });
    clock.t = 10_999; b.state.sweep(); expect(b.state.retained).toEqual([1n, 2n]);
    clock.t = 11_000; b.state.sweep(); expect(b.state.retained).toEqual([2n]);
    // Barrier at 0, import delayed to 3 s and retried with new serials: deadline still 10 s.
    const c2 = new Clock(); c2.t = 0;
    const c = engine(c2);
    await c.enter(1n, 1, members(1n, [[0, 0], [1, 1]]));
    await c.ok({ action: "supersede", epoch: "2" });
    c2.t = 3_000;
    await c.ok({ action: "supersede", epoch: "3" }); // a later chain epoch preserves the first time
    expect(await c.send({ action: "install", epoch: "2", own_leaf: 1, members: members(2n, [[0, 0], [1, 1]]), removed: [] })).toMatchObject({ result: "ignored" });
    await c.ok({ action: "install", epoch: "3", own_leaf: 1, members: members(3n, [[0, 0], [1, 1]]), removed: [] });
    c2.t = 9_999; c.state.sweep(); expect(c.state.retained).toEqual([1n, 3n]);
    c2.t = 10_000; c.state.sweep(); expect(c.state.retained).toEqual([3n]);
    expect(c.state.counters.stale_install).toBe(1);
  });

  it("evicts the ninth epoch regardless of retention", async () => {
    const b = engine();
    for (let e = 1n; e <= 9n; e++) await b.enter(e, 1, members(e, [[0, 0], [1, 1]]));
    expect(b.state.retained).toEqual([2n, 3n, 4n, 5n, 6n, 7n, 8n, 9n]);
  });

  it("orders controls by serial and treats duplicates and conflicts exactly", async () => {
    const b = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await b.ok({ action: "supersede", epoch: "1" });
    const install = { binding, serial: b.nextSerial(), action: "install", epoch: "1", own_leaf: 1, members: all, removed: [] };
    expect(await b.state.control(install)).toMatchObject({ result: "applied" });
    expect(await b.state.control(install)).toMatchObject({ result: "unchanged" });
    expect(await b.state.control({ ...install, serial: "1" })).toMatchObject({ result: "ignored" }); // lower serial
    // A newer serial with identical keys preserves the counter and replay windows.
    await b.ok({ action: "activate", epoch: "1", send_until: null });
    const first = (await b.state.encrypt(OPUS.slice()))!;
    expect(await b.send({ action: "install", epoch: "1", own_leaf: 1, members: all, removed: [] })).toMatchObject({ result: "unchanged" });
    const second = (await b.state.encrypt(OPUS.slice()))!;
    expect(decodeHeader(second)!.ctr).toBe(decodeHeader(first)!.ctr + 1n);
    // Different keys at the installed epoch stop the engine.
    const other = engine();
    await other.ok({ action: "supersede", epoch: "1" });
    await other.ok({ action: "install", epoch: "1", own_leaf: 1, members: all, removed: [] });
    expect(await other.send({ action: "install", epoch: "1", own_leaf: 1, members: members(1n, [[0, 0], [1, 1]], 9), removed: [] })).toMatchObject({ error: "key_conflict" });
    expect(other.statuses).toEqual([{ binding, state: "failed", reason: "key_conflict" }]);
    expect(other.state.stopped).toBe(true);
    // A different body at the same serial stops it as well, for a barrier too.
    const third = engine();
    const barrier = { binding, serial: "1", action: "supersede", epoch: "1" };
    await third.state.control(barrier);
    expect(await third.state.control({ ...barrier, epoch: "2" })).toMatchObject({ error: "key_conflict" });
  });

  it("refuses future, malformed and unbound controls without mutation", async () => {
    const b = engine();
    expect(await b.send({ action: "install", epoch: "1", own_leaf: 1, members: members(1n, [[0, 0], [1, 1]]), removed: [] })).toMatchObject({ error: "bad_keys" });
    expect(await b.send({ action: "activate", epoch: "1", send_until: null })).toMatchObject({ error: "not_installed" });
    const valid: Control = { binding, serial: "50", action: "install", epoch: "1", own_leaf: 1, members: members(1n, [[0, 0], [1, 1]]), removed: [] };
    expect(parseControl(valid)).not.toBeNull();
    for (const bad of [
      { ...valid, serial: "0" }, { ...valid, serial: "01" }, { ...valid, epoch: "18446744073709551616" }, { ...valid, epoch: 1 },
      { ...valid, own_leaf: 2 }, { ...valid, members: [] }, { ...valid, members: [...valid.members].reverse() },
      { ...valid, members: [valid.members[0], { ...valid.members[1]!, device: valid.members[0]!.device }] },
      { ...valid, members: [valid.members[0], { ...valid.members[1]!, key: filled(31, 1) }] },
      { ...valid, removed: [DEVICES[0], DEVICES[0]] }, { ...valid, extra: 1 }, { ...valid, binding: { ...binding, run: 0 } },
      { ...valid, binding: { ...binding, call: filled(15, 1) } }, { binding, serial: "1", action: "activate", epoch: "1", send_until: 1.5 },
      { binding, serial: "1", action: "reset", epoch: "1" },
    ]) expect(parseControl(bad), JSON.stringify(bad).slice(0, 80)).toBeNull();
    expect(await b.state.control({ ...valid, serial: "2" }, 256 * 1024 + 1)).toMatchObject({ error: "bad_keys" });
    // Bad structure did not consume serial 50; a foreign binding is ignored.
    expect(await b.state.control({ ...valid, action: "supersede", own_leaf: undefined, members: undefined, removed: undefined, serial: "3" } as unknown)).toMatchObject({ error: "bad_keys" });
    expect(await b.state.control({ binding: { ...binding, run: 2 }, serial: "60", action: "supersede", epoch: "1" })).toMatchObject({ result: "ignored" });
    expect(await b.state.control({ binding, serial: "4", action: "supersede", epoch: "1" })).toMatchObject({ result: "applied" });
  });

  it("refuses installs at or below a restart's resume floor", async () => {
    const b = engine(new Clock(), 4n);
    expect(await b.send({ action: "supersede", epoch: "4" })).toMatchObject({ result: "ignored" });
    await b.ok({ action: "supersede", epoch: "5" });
    await b.ok({ action: "install", epoch: "5", own_leaf: 1, members: members(5n, [[0, 0], [1, 1]]), removed: [] });
  });

  it("closes sending at certificate expiry without extending it on clock rollback", async () => {
    const clock = new Clock();
    const a = engine(clock);
    const all = members(1n, [[0, 0], [1, 1]]);
    await a.ok({ action: "supersede", epoch: "1" });
    await a.ok({ action: "install", epoch: "1", own_leaf: 0, members: all, removed: [] });
    await a.ok({ action: "activate", epoch: "1", send_until: clock.wall + 1_000 });
    expect(await a.state.encrypt(OPUS.slice())).not.toBeNull();
    clock.wall -= 60_000; // rollback, then a newer activation of the same epoch
    expect(await a.send({ action: "activate", epoch: "1", send_until: clock.wall + 61_000 })).toMatchObject({ result: "unchanged" });
    clock.t += 1_000;
    expect(await a.state.encrypt(OPUS.slice())).toBeNull();
    expect(a.statuses).toEqual([{ binding, state: "securing", reason: "key_expired" }]);
    expect(a.state.sending).toBe(false);
  });

  it("holds certificate expiry while the monotonic clock is stopped (sleep)", async () => {
    const clock = new Clock();
    const a = engine(clock);
    await a.ok({ action: "supersede", epoch: "1" });
    await a.ok({ action: "install", epoch: "1", own_leaf: 0, members: members(1n, [[0, 0], [1, 1]]), removed: [] });
    await a.ok({ action: "activate", epoch: "1", send_until: clock.wall + 500 });
    expect(await a.state.encrypt(OPUS.slice())).not.toBeNull();
    clock.wall += 2_000; // the machine slept: wall time moved, the monotonic clock did not
    expect(await a.state.encrypt(OPUS.slice())).toBeNull();
    expect(a.statuses).toEqual([{ binding, state: "securing", reason: "key_expired" }]);
  });

  it("drops encryption in flight when a hold arrives and never reuses a counter", async () => {
    const a = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await a.enter(1n, 0, all);
    const seen = new Set<bigint>();
    const pending = a.state.encrypt(OPUS.slice());
    await a.ok({ action: "hold", epoch: "1", removed: [] });
    expect(await pending).toBeNull();
    expect(await a.state.encrypt(OPUS.slice())).toBeNull();
    await a.ok({ action: "activate", epoch: "1", send_until: null });
    for (let i = 0; i < 5; i++) {
      const ctr = decodeHeader((await a.state.encrypt(OPUS.slice()))!)!.ctr;
      expect(seen.has(ctr)).toBe(false); seen.add(ctr);
    }
    expect([...seen]).toEqual([1n, 2n, 3n, 4n, 5n]); // counter 0 was reserved by the dropped attempt
  });

  it("stops at 2^32 attempts under one base key", async () => {
    const a = engine();
    await a.enter(1n, 0, members(1n, [[0, 0], [1, 1]]));
    (a.state as unknown as { sender: { counter: bigint } }).sender.counter = COUNTER_LIMIT - 1n;
    expect(decodeHeader((await a.state.encrypt(OPUS.slice()))!)!.ctr).toBe(COUNTER_LIMIT - 1n);
    expect(await a.state.encrypt(OPUS.slice())).toBeNull();
    expect(a.statuses).toEqual([{ binding, state: "securing", reason: "counter_exhausted" }]);
    expect(a.state.counters.counter_exhausted).toBe(1);
  });

  it("ignores a superseded import and a device removed while importing", async () => {
    let release!: () => void;
    const gate = new Promise<void>(r => { release = r; });
    const slow: SubtleCrypto = new Proxy(crypto.subtle, { get: (t, p) => p === "importKey" ? async (...args: unknown[]) => { await gate; return (t.importKey as (...a: unknown[]) => unknown).apply(t, args); } : (t as unknown as Record<string | symbol, unknown>)[p] instanceof Function ? ((t as unknown as Record<string | symbol, (...a: unknown[]) => unknown>)[p]!).bind(t) : (t as unknown as Record<string | symbol, unknown>)[p] });
    const clock = new Clock();
    const b = new SframeState({ binding, resumeAfter: null, now: clock.now, wallNow: clock.wallNow, onStatus: () => {}, subtle: slow });
    await b.control({ binding, serial: "1", action: "supersede", epoch: "1" });
    const install = b.control({ binding, serial: "2", action: "install", epoch: "1", own_leaf: 1, members: members(1n, [[0, 0], [1, 1], [2, 2]]), removed: [] });
    expect(await b.control({ binding, serial: "3", action: "hold", epoch: "1", removed: [DEVICES[2]!] })).toMatchObject({ result: "applied" });
    expect(await b.control({ binding, serial: "4", action: "supersede", epoch: "2" })).toMatchObject({ result: "applied" });
    release();
    expect(await install).toMatchObject({ result: "ignored" });
    expect(b.retained).toEqual([]);
    // Same with no newer barrier: the purged device stays out.
    const c = engine();
    let open!: () => void;
    const g2 = new Promise<void>(r => { open = r; });
    const slow2: SubtleCrypto = new Proxy(crypto.subtle, { get: (t, p) => p === "importKey" ? async (...args: unknown[]) => { await g2; return (t.importKey as (...a: unknown[]) => unknown).apply(t, args); } : ((t as unknown as Record<string | symbol, unknown>)[p] as (...a: unknown[]) => unknown).bind(t) });
    const s = new SframeState({ binding, resumeAfter: null, now: c.clock.now, wallNow: c.clock.wallNow, onStatus: () => {}, subtle: slow2 });
    await s.control({ binding, serial: "1", action: "supersede", epoch: "1" });
    const done = s.control({ binding, serial: "2", action: "install", epoch: "1", own_leaf: 1, members: members(1n, [[0, 0], [1, 1], [2, 2]]), removed: [] });
    await s.control({ binding, serial: "3", action: "hold", epoch: "1", removed: [DEVICES[2]!] });
    open();
    expect(await done).toMatchObject({ result: "applied" });
    const d2 = engine();
    await d2.enter(1n, 2, members(1n, [[0, 0], [1, 1], [2, 2]]));
    expect(await s.decrypt((await d2.state.encrypt(OPUS.slice()))!)).toBeNull();
    expect(s.counters.no_key).toBe(1);
  });

  it("fails the run when more than eight controls are pending", async () => {
    let release!: () => void;
    const gate = new Promise<void>(r => { release = r; });
    const slow: SubtleCrypto = new Proxy(crypto.subtle, { get: (t, p) => p === "importKey" ? async (...args: unknown[]) => { await gate; return (t.importKey as (...a: unknown[]) => unknown).apply(t, args); } : ((t as unknown as Record<string | symbol, unknown>)[p] as (...a: unknown[]) => unknown).bind(t) });
    const statuses: CryptoStatus[] = [];
    const clock = new Clock();
    const s = new SframeState({ binding, resumeAfter: null, now: clock.now, wallNow: clock.wallNow, onStatus: x => statuses.push(x), subtle: slow });
    const waits = [];
    let serial = 0;
    for (let e = 1; e <= 8; e++) {
      await s.control({ binding, serial: String(++serial), action: "supersede", epoch: String(e) });
      waits.push(s.control({ binding, serial: String(++serial), action: "install", epoch: String(e), own_leaf: 1, members: members(BigInt(e), [[0, 0], [1, 1]]), removed: [] }));
    }
    expect(await s.control({ binding, serial: String(serial + 1), action: "supersede", epoch: "9" })).toMatchObject({ error: "key_queue_full" });
    expect(statuses).toEqual([{ binding, state: "failed", reason: "key_queue_full" }]);
    release();
    for (const w of await Promise.all(waits)) expect(w).toMatchObject({ result: "ignored" });
  });

  it("compares installs by normalized members and keeps no raw exporter key", async () => {
    const b = engine();
    const all = members(1n, [[0, 0], [1, 1]]);
    await b.enter(1n, 1, all);
    const reordered = all.map(m => ({ key: m.key, device: m.device, leaf: m.leaf }));
    expect(await b.send({ action: "install", epoch: "1", own_leaf: 1, members: reordered, removed: [] })).toMatchObject({ result: "unchanged" });
    expect(b.state.stopped).toBe(false);
    const internals = JSON.stringify(b.state, (_, v) => (typeof v === "bigint" ? v.toString() : v instanceof Map ? [...v] : v));
    for (const m of all) expect(internals).not.toContain(m.key);
  });

  it("drops an encryption that completes after the certificate deadline", async () => {
    const clock = new Clock();
    let release!: () => void;
    const gate = new Promise<void>(r => { release = r; });
    const slow: SubtleCrypto = new Proxy(crypto.subtle, { get: (t, p) => p === "encrypt" ? async (...args: unknown[]) => { await gate; return (t.encrypt as (...a: unknown[]) => unknown).apply(t, args); } : ((t as unknown as Record<string | symbol, unknown>)[p] as (...a: unknown[]) => unknown).bind(t) });
    const statuses: CryptoStatus[] = [];
    const s = new SframeState({ binding, resumeAfter: null, now: clock.now, wallNow: clock.wallNow, onStatus: x => statuses.push(x), subtle: slow });
    await s.control({ binding, serial: "1", action: "supersede", epoch: "1" });
    await s.control({ binding, serial: "2", action: "install", epoch: "1", own_leaf: 0, members: members(1n, [[0, 0], [1, 1]]), removed: [] });
    await s.control({ binding, serial: "3", action: "activate", epoch: "1", send_until: clock.wall + 1_000 });
    const pending = s.encrypt(OPUS.slice());
    clock.t += 1_000;
    release();
    expect(await pending).toBeNull();
    expect(statuses).toEqual([{ binding, state: "securing", reason: "key_expired" }]);
  });

  it("closes the run when a key import fails", async () => {
    const broken: SubtleCrypto = new Proxy(crypto.subtle, { get: (t, p) => p === "importKey" ? async () => { throw new Error("import failed"); } : ((t as unknown as Record<string | symbol, unknown>)[p] as (...a: unknown[]) => unknown).bind(t) });
    const statuses: CryptoStatus[] = [];
    const clock = new Clock();
    const s = new SframeState({ binding, resumeAfter: null, now: clock.now, wallNow: clock.wallNow, onStatus: x => statuses.push(x), subtle: broken });
    await s.control({ binding, serial: "1", action: "supersede", epoch: "1" });
    expect(await s.control({ binding, serial: "2", action: "install", epoch: "1", own_leaf: 0, members: members(1n, [[0, 0], [1, 1]]), removed: [] })).toMatchObject({ result: "ignored" });
    expect(statuses).toEqual([{ binding, state: "failed", reason: "worker_failed" }]);
    expect(s.stopped).toBe(true);
  });

  it("a removed device that keeps its keys opens nothing encrypted after the sender's cutoff", async () => {
    const a = engine(), c = engine();
    const one = members(1n, [[0, 0], [1, 1], [2, 2]]);
    await a.enter(1n, 0, one); await c.enter(1n, 2, one);
    const before = (await a.state.encrypt(OPUS.slice()))!;
    expect(await c.state.decrypt(before)).toEqual(OPUS); // the design's stated removal limit
    // Carol is removed: the sender holds, then moves to epoch 2 without her.
    await a.ok({ action: "hold", epoch: "2", removed: [DEVICES[2]!] });
    expect(await a.state.encrypt(OPUS.slice())).toBeNull(); // nothing is encrypted between hold and activation
    await a.enter(2n, 0, members(2n, [[0, 0], [1, 1]]), [DEVICES[2]!]);
    for (let i = 0; i < 5; i++) expect(await c.state.decrypt((await a.state.encrypt(OPUS.slice()))!)).toBeNull();
    expect(c.state.counters.no_key).toBe(5);
    expect(c.state.counters.decrypted).toBe(1);
  });

  it("encrypts with exactly the RFC header for its kid and counter", async () => {
    const a = engine();
    await a.enter(3n, 4, members(3n, [[0, 0], [4, 1]]));
    const frame = (await a.state.encrypt(OPUS.slice()))!;
    expect(frame.subarray(0, encodeHeader(voiceKid(3n, 4), 0n).length)).toEqual(encodeHeader(voiceKid(3n, 4), 0n));
  });
});
