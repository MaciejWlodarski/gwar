import { afterEach, describe, expect, it, vi } from "vitest";
import { IDBFactory } from "fake-indexeddb";
import { MlsRuntime } from "./runtime";
import { RequestError, type Link, type ConnectionStatus } from "../net/connection";
import { encodeBase64Url } from "../net/base64url";
import type { Event } from "../proto/Event";
import type { Identity } from "../net/identity";
import { openStorage } from "./storage";
import { emptyChanges, type AgentFactory, type CommitOutbox, type GroupSummary, type WasmAgent, type Plaintext } from "./types";
vi.mock("./wasm", () => ({ loadAgent: vi.fn() }));
const enc = new TextEncoder(), dec = new TextDecoder();
const b64 = (n: number, length = 32) => encodeBase64Url(new Uint8Array(length).fill(n));
const server = b64(9), group = b64(2), device = b64(1), peer = b64(3);
const conversation = `dm:${[b64(4,20), b64(5,20)].sort().join(":")}`;
interface State { groups: GroupSummary[]; out: CommitOutbox | null; welcomes: string[]; imports: number; shares: number }
const summary = (status: GroupSummary["record"]["status"] = "active", epoch = "0"): GroupSummary => ({ group: Array(32).fill(2), conversation, history: true, record: { status, checkpoint: status === "creating" ? null : { epoch, hash: [] }, server_pending: false }, members: [{ device_key: device, uid: b64(4,20) }] });
const pending = (): CommitOutbox => ({ group: Array(32).fill(2), epoch: "0", hash: [4], commit: [4], group_info: [5], welcome: [6] });
const clean = (): State => ({ groups: [], out: null, welcomes: [], imports: 0, shares: 0 });
const runtimes: MlsRuntime[] = [];
afterEach(async () => { await Promise.all(runtimes.splice(0).map(r => r.close())); });
async function fixture(initial = clean(), hasRemoteGroup = true) {
  const operations: Record<string, unknown>[] = [], calls: { op: string; data: Record<string, unknown> }[] = [];
  let current!: FakeAgent;
  class FakeAgent implements WasmAgent {
    constructor(readonly state: State) {
      // eslint-disable-next-line @typescript-eslint/no-this-alias -- inspect the live fake after reload
      current = this;
    }
    execute(request: unknown): unknown {
      const r = request as Record<string, Record<string, unknown>>;
      const [op, d] = Object.entries(r)[0]!;
      operations.push({ op, ...d });
      const g = this.state.groups[0];
      switch (op) {
        case "inventory": return structuredClone(this.state.groups);
        case "device_hpke_key": return { hpke_key: b64(7), created_at: 1, signature: b64(8,64) };
        case "gate": return { status: g?.record.status, rejoin: false, pending_commit: !!this.state.out, server_pending: g?.record.server_pending, blocked: [] };
        case "outbox": return this.state.out;
        case "sending_key": return [g!.record.checkpoint!.epoch, Array(32).fill(1)];
        case "seal_message": return [9];
        case "pending": g!.record.server_pending = d!.pending as boolean; return null;
        case "settle": if (d!.winner) g!.record = summary().record; else this.state.groups = []; return !!d!.winner;
        case "welcome": {
          const duplicate = this.state.welcomes.includes(d!.id as string);
          if (!duplicate) { this.state.groups = [summary("active", "1")]; this.state.welcomes.push(d!.id as string); this.state.groups[0]!.record.welcome_ids = [...this.state.welcomes]; }
          return { group: Array(32).fill(2), duplicate };
        }
        case "reconcile": {
          const result = d!.result as { case: string; commit?: number[]; reason?: string };
          if (result.case === "head" || (result.case === "rejected" && result.reason !== "expired")) return "retry";
          const epoch = String(Number(this.state.out?.epoch ?? 0) + 1);
          this.state.out = null;
          if (result.case === "pruned") { g!.record.status = "rejoin"; return "rejoin"; }
          if (result.case === "rejected") return "rebuild";
          g!.record.checkpoint!.epoch = epoch;
          g!.record.status = "active";
          return result.commit?.[0] === 4 ? "accepted" : "rebuild";
        }
        case "receive": g!.record.checkpoint!.epoch = String(Number(g!.record.checkpoint!.epoch) + 1); g!.members.push({ device_key: peer, uid: b64(5,20) }); return null;
        case "join_external": this.state.groups = [summary("joining", "1")]; this.state.out = { ...pending(), epoch: "1" }; return this.state.out;
        case "key_packages": return [[1], [2], [3], [4], [5]];
        case "last_resort_key_package": return [6];
        case "share_history": this.state.shares++; return [[22], [23]];
        case "import_history": this.state.imports++; if ((d!.bytes as number[])[0] === 255) throw new Error("invalid history"); return null;
        case "open_message": return { reason: "no_key" };
        case "epoch": return null;
        default: throw new Error(`unexpected fake agent op ${op}`);
      }
    }
    take_changes() { const c = emptyChanges(); c.groups = [{ key: [1], value: [...enc.encode(JSON.stringify(this.state))] }]; return c; }
    free() {}
  }
  const factory: AgentFactory = { create: () => new FakeAgent(structuredClone(initial)), load: c => new FakeAgent(JSON.parse(dec.decode(new Uint8Array(c.groups[0]!.value!)))) };
  const storage = await openStorage(crypto.randomUUID(), new IDBFactory());
  const durable = async (): Promise<State> => JSON.parse(dec.decode(new Uint8Array((await storage.load()).groups[0]!.value!)));
  const world = {
    groups: initial.groups.length && hasRemoteGroup ? [{ conversation, group, epoch: 0, pending: false }] : [],
    head: 0, oldest: 0,
    entries: [] as { epoch: number; commit: string; proposals: string[] }[],
    welcomes: [] as { id: string; group: string; welcome: string }[],
    chunks: [] as { id: string; group: string; sender_uid: string; chunk: string }[],
    wanted: [] as { group: string; device: string }[],
    commitError: null as RequestError | null, shareError: false, ackError: false,
    beforeLog: null as null | (() => Promise<void>),
    packages: 5, devices: [] as { uid: string; device_key: string; hpke_key: string; created_at: number; signature: string }[],
  };
  let welcomeHandler = () => {};
  let eventHandler = (_event: Event) => {};
  let statusHandler = (_status: ConnectionStatus) => {};
  const link = {
    status: { state: "idle" }, isOnline: false, serverKey: server,
    onStatus: (fn: (status: ConnectionStatus) => void) => { statusHandler = fn; return () => {}; }, onEvent: (fn: (event: Event) => void) => { eventHandler = fn; return () => {}; }, onWelcome: (fn: () => void) => { welcomeHandler = fn; return () => {}; },
    connect: async () => { link.status.state = "online"; link.isOnline = true; welcomeHandler(); return {} as never; },
    close: () => { link.isOnline = false; },
    request: async (op: string, data: Record<string, unknown>) => {
      calls.push({ op, data });
      switch (op) {
        case "mls.device.publish": return {};
        case "mls.key_packages.publish": if ((data.packages as string[]).length) world.packages = 5; return { count: world.packages };
        case "mls.groups": return { groups: world.groups, next: null };
        case "mls.welcomes": return { welcomes: world.welcomes, next: null };
        case "mls.welcomes.ack": expect((await durable()).welcomes).toContain((data.ids as string[])[0]); if (world.ackError) { world.ackError = false; throw new Error("lost ack"); } world.welcomes = []; return {};
        case "mls.log": await world.beforeLog?.(); return { head: world.head, oldest: world.oldest, entries: world.entries.filter(e => e.epoch >= Number(data.since)), pending: [], next: null };
        case "mls.group_info": return { group_info: b64(8), epoch: world.head };
        case "mls.commit": expect((await durable()).out?.commit).toEqual([4]); if (world.commitError) throw world.commitError; world.entries.push({ epoch: Number(data.epoch), commit: data.commit as string, proposals: [] }); world.head = Number(data.epoch) + 1; return { epoch: world.head };
        case "mls.devices": return { devices: world.devices, next: null };
        case "mls.history.fetch": return { chunks: world.chunks, next: null };
        case "mls.history.ack": expect((await durable()).imports).toBeGreaterThan(0); world.chunks = world.chunks.filter(c => !(data.ids as string[]).includes(c.id)); return {};
        case "mls.history.wanted": return { wanted: world.wanted, next: null };
        case "mls.history.request": return {};
        case "mls.history.share": expect((await storage.load()).outbox.length).toBeGreaterThan(0); if (world.shareError) throw new Error("lost share response"); return {};
        default: throw new Error(`unexpected request ${op}`);
      }
    },
  };
  const identity = { publicKey: device, device: undefined } as unknown as Identity;
  const locks = { request: async (_name: string, _options: unknown, fn: () => Promise<void>) => fn() } as unknown as LockManager;
  const runtime = await MlsRuntime.connect({ identity, seed: new Uint8Array(32), link: link as unknown as Link, revocations: () => [], locks, storage: async () => storage, factory });
  runtimes.push(runtime);
  // Initialization has completed, so changes below model subsequent reconnects/events.
  return { runtime, world, calls, operations, durable, state: () => current.state, storage,
    proposal: () => eventHandler({ ev: "mls.proposal", d: { group, epoch: 0, proposal: "AQ" } }),
    persist: () => storage.write(current.take_changes()),
    reconnect: () => {
      link.isOnline = false; statusHandler({ state: "reconnecting", attempt: 1, retryAt: Date.now() });
      link.isOnline = true; statusHandler({ state: "online" });
    },
  };
}
describe("MLS synchronization", () => {
  it("settles persisted Creating groups from the authoritative group list", async () => {
    const f = await fixture({ ...clean(), groups: [summary("creating")] });
    expect(f.operations.some(op => op.op === "settle")).toBe(true);
    expect((await f.durable()).groups[0]!.record.status).toBe("active");
  });
  it("settles away a Creating group absent after restart", async () => {
    const f = await fixture({ ...clean(), groups: [summary("creating")] }, false);
    expect((await f.durable()).groups).toEqual([]);
    expect(f.operations.some(op => op.op === "settle" && op.winner === null)).toBe(true);
  });
  it("deduplicates a Welcome after its durable write and a lost ack", async () => {
    const f = await fixture(); f.world.welcomes = [{ id: "w1", group, welcome: "AQ" }]; f.world.ackError = true;
    await expect(f.runtime.sync()).rejects.toThrow("lost ack");
    expect((await f.durable()).welcomes).toEqual(["w1"]);
    await f.runtime.sync(); expect(f.state().welcomes).toEqual(["w1"]);
    expect(f.calls.filter(c => c.op === "mls.welcomes.ack")).toHaveLength(2);
  });
  it("replays identical outbox bytes when the log head has not advanced", async () => {
    const f = await fixture({ ...clean(), groups: [summary()], out: pending() });
    expect(f.calls.filter(c => c.op === "mls.commit").map(c => c.data.commit)).toEqual(["BA"]);
    expect((await f.durable()).out).toBeNull();
  });
  it.each(["early", "bad_request", "stale"] as const)("keeps a %s rejected commit for retry", async code => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.state().out = pending(); f.world.commitError = new RequestError(code, code);
    await expect(f.runtime.sync()).rejects.toThrow(code);
    expect((await f.durable()).out?.commit).toEqual([4]);
    f.world.commitError = null; await f.runtime.sync(); expect((await f.durable()).out).toBeNull();
  });
  it("uses logged winner entries without resending, and expires only on authoritative rejection", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.state().out = pending(); f.world.head = 1; f.world.entries = [{ epoch: 0, commit: "CQ", proposals: ["AQ"] }];
    await f.runtime.sync(); expect(f.calls.filter(c => c.op === "mls.commit")).toHaveLength(0);
    expect(f.operations.some(o => o.op === "reconcile" && (o.result as { case: string }).case === "entry")).toBe(true);
    f.state().out = { ...pending(), epoch: "1" }; f.world.commitError = new RequestError("expired", "expired");
    await f.runtime.sync(); expect((await f.durable()).out).toBeNull();
  });
  it("rejoins when the pending epoch was pruned", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.state().out = pending(); f.world.head = 1; f.world.oldest = 1;
    await f.runtime.sync();
    expect(f.operations.some(o => o.op === "reconcile" && (o.result as { case: string }).case === "pruned")).toBe(true);
    expect(f.operations.some(o => o.op === "join_external")).toBe(true);
    expect((await f.durable()).groups[0]!.record.checkpoint!.epoch).toBe("2");
  });
  it("does not send an old operation on a replacement connection", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.state().out = pending(); await f.persist();
    let release!: () => void, started!: () => void;
    const waiting = new Promise<void>(resolve => { release = resolve; });
    const began = new Promise<void>(resolve => { started = resolve; });
    f.world.beforeLog = async () => { started(); await waiting; };
    const syncing = f.runtime.sync(); await began;
    f.reconnect(); release();
    await expect(syncing).rejects.toThrow("connection changed");
    expect(f.calls.filter(c => c.op === "mls.commit")).toHaveLength(0);
    expect((await f.durable()).out?.commit).toEqual([4]);
    f.world.beforeLog = null; await f.runtime.sync();
    expect((await f.durable()).out).toBeNull();
  });
  it("does not seal when a proposal arrives while awaiting the signature", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    await expect(f.runtime.sealMessage(conversation, async () => {
      f.proposal(); return {} as Plaintext;
    })).rejects.toThrow("group changed");
    expect(f.operations.some(o => o.op === "seal_message")).toBe(false);
  });
  it("shares admitted history with persisted chunks, retries identical bytes, imports before ack", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.head = 1; f.world.entries = [{ epoch: 0, commit: "CQ", proposals: [] }];
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.shareError = true;
    await expect(f.runtime.sync()).rejects.toThrow("lost share response");
    expect(f.state().shares).toBe(1);
    f.world.shareError = false; await f.runtime.sync();
    expect(f.state().shares).toBe(1); // Do not reseal and evade server deduplication.
    expect(f.calls.filter(c => c.op === "mls.history.share").map(c => c.data.chunk)).toEqual(["Fg", "Fg", "Fw"]);
    f.world.chunks = [{ id: "h1", group, sender_uid: b64(5,20), chunk: "AQ" }];
    await f.runtime.sync(); expect(f.state().imports).toBe(1); expect(f.world.chunks).toEqual([]);
  });
  it("reloads after invalid history and imports other chunks without acknowledging the bad one", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.chunks = [
      { id: "bad", group, sender_uid: b64(5,20), chunk: "_w" },
      { id: "good", group, sender_uid: b64(5,20), chunk: "AQ" },
    ];
    await f.runtime.sync();
    expect(f.state().imports).toBe(1);
    expect(f.world.chunks.map(c => c.id)).toEqual(["bad"]);
    expect(f.calls.filter(c => c.op === "mls.history.ack").map(c => c.data.ids)).toEqual([["good"]]);
  });
  it("requests missing keys, throttles repeated requests, and answers wanted history", async () => {
    const local = summary(); local.members.push({ device_key: peer, uid: b64(5,20) });
    const f = await fixture({ ...clean(), groups: [local] });
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.wanted = [{ group, device: peer }];
    expect(await f.runtime.openMessage({ conversation, group, epoch: 0, message_id: b64(2,16), revision: 0, ciphertext: "AQ" })).toEqual({ reason: "no_key" });
    await f.runtime.sync(); await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(1);
    expect(f.calls.some(c => c.op === "mls.history.share")).toBe(true);
  });
});
