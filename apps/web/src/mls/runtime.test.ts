import { afterEach, describe, expect, it, vi } from "vitest";
import { IDBFactory } from "fake-indexeddb";
import { MlsRuntime } from "./runtime";
import { RequestError, type Link, type ConnectionStatus } from "../net/connection";
import { encodeBase64Url } from "../net/base64url";
import type { Event } from "../proto/Event";
import { uidForPublicKey } from "../net/identity";
import type { Identity } from "../net/identity";
import { openStorage } from "./storage";
import { emptyChanges, type AgentFactory, type CommitOutbox, type GroupSummary, type WasmAgent, type Plaintext, type Revocation } from "./types";
vi.mock("./wasm", () => ({ loadAgent: vi.fn() }));
const enc = new TextEncoder(), dec = new TextDecoder();
const b64 = (n: number, length = 32) => encodeBase64Url(new Uint8Array(length).fill(n));
const server = b64(9), group = b64(2), device = b64(1), peer = b64(3);
const dm = `dm:${[b64(4,20), b64(5,20)].sort().join(":")}`;
let conversation = dm;
interface State { groups: GroupSummary[]; out: CommitOutbox | null; welcomes: string[]; imports: number; shares: number }
const summary = (status: GroupSummary["record"]["status"] = "active", epoch = "0"): GroupSummary => ({ group: Array(32).fill(2), conversation, incarnation: "0", live: status !== "creating", history: true, record: { status, checkpoint: status === "creating" ? null : { epoch, hash: [] }, server_pending: false, own_commit_at: Date.now() }, members: [{ device_key: device, uid: b64(4,20) }] });
const pending = (): CommitOutbox => ({ group: Array(32).fill(2), epoch: "0", hash: [4], commit: [4], group_info: [5], welcome: [6] });
const clean = (): State => ({ groups: [], out: null, welcomes: [], imports: 0, shares: 0 });
const runtimes: MlsRuntime[] = [];
afterEach(async () => { conversation = dm; await Promise.all(runtimes.splice(0).map(r => r.close())); });
interface Options { serverChat?: boolean; channels?: { id: number; encrypted: boolean; history: boolean }[]; now?: () => number }
async function fixture(initial = clean(), hasRemoteGroup = true, options: Options = {}) {
  const errors: unknown[] = [];
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
      // The named group, else the fixture's single group.
      const named = d?.group as ArrayLike<number> | undefined;
      const g = (named && this.state.groups.find(x => x.group.join() === Array.from(named).join())) || this.state.groups[0];
      switch (op) {
        case "inventory": return structuredClone(this.state.groups);
        case "device_hpke_key": return { hpke_key: b64(7), created_at: 1, signature: b64(8,64) };
        case "gate": return { status: g?.record.status, rejoin: false, pending_commit: !!this.state.out, server_pending: g?.record.server_pending, blocked: g?.members.filter(m => world.revocations.some(r => r.device_key === m.device_key)).map((_, i) => i) ?? [] };
        case "outbox": return this.state.out;
        case "sending_key": return [g!.record.checkpoint!.epoch, Array(32).fill(1)];
        case "seal_message": return [9];
        case "pending": g!.record.server_pending = d!.pending as boolean; return null;
        case "settle": if (d!.winner) g!.record = summary().record; else this.state.groups = []; return !!d!.winner;
        case "welcome":
        case "replace_stale_welcome": {
          const duplicate = this.state.welcomes.includes(d!.id as string);
          if (!duplicate && g && ["active", "joining", "creating"].includes(g.record.status)) {
            if (op === "welcome") throw new Error("group already joined");
            if (Number((d!.bytes as number[])[0]) <= Number(g.record.checkpoint!.epoch)) throw new Error("stale Welcome");
          }
          if (!duplicate) { this.state.out = null; this.state.groups = [summary("active", String((d!.bytes as number[])[0]))]; this.state.welcomes.push(d!.id as string); this.state.groups[0]!.record.welcome_ids = [...this.state.welcomes]; }
          return { group: Array(32).fill(2), duplicate };
        }
        case "reconcile": {
          const result = d!.result as { case: string; commit?: number[]; reason?: string };
          if (result.case === "head" || (result.case === "rejected" && result.reason !== "expired")) return "retry";
          const epoch = String(Number(this.state.out?.epoch ?? 0) + 1);
          this.state.out = null;
          if (result.case === "pruned") { g!.record.status = "rejoin"; return "rejoin"; }
          if (result.case === "rejected") return "rebuild";
          // A member's pending commit that lost to its removal.
          if (g!.record.status === "active" && result.commit?.[0] === 99) { g!.record.status = "removed"; g!.members = []; return "rejoin"; }
          // A pending join that lost is dropped, the winning entry unprocessed.
          if (g!.record.status === "joining" && result.commit?.[0] !== 4) { g!.record.status = "rejoin"; return "rejoin"; }
          g!.record.checkpoint!.epoch = epoch;
          g!.record.status = "active";
          g!.members = g!.members.filter(m => !world.revocations.some(r => r.device_key === m.device_key));
          return result.commit?.[0] === 4 ? "accepted" : "rebuild";
        }
        case "commit": this.state.out = { ...pending(), epoch: g!.record.checkpoint!.epoch }; return this.state.out;
        case "receive": if ((d!.commit as number[])[0] === 99) { g!.record.status = "removed"; g!.members = []; return null; } g!.record.checkpoint!.epoch = String(Number(g!.record.checkpoint!.epoch) + 1); g!.members.push({ device_key: peer, uid: b64(5,20) }); return null;
        case "join_external": {
          if (world.joinError) throw new Error("join failed");
          // The live pin still to be joined after a restart, or the fixture's group.
          const target = this.state.groups.find(x => x.live && x.record.status === "rejoin");
          const joined = { ...summary("joining", String(world.head)), ...(target && { group: target.group, incarnation: target.incarnation }) };
          this.state.groups = [...this.state.groups.filter(x => x.record.status === "superseded"), joined];
          this.state.out = { ...pending(), group: joined.group, epoch: String(world.head) }; return this.state.out;
        }
        case "key_packages": return [[1], [2], [3], [4], [5]];
        case "last_resort_key_package": return [6];
        case "share_history": if ((d!.recipient as { signature: string }).signature === "invalid") throw new Error("invalid HPKE signature"); this.state.shares++; return world.nothingToShare ? [] : [[21 + this.state.shares], [22 + this.state.shares]];
        case "create": this.state.groups = [summary("creating")]; return [7];
        case "forget": this.state.groups = []; this.state.out = null; return null;
        case "import_history": this.state.imports++; if ((d!.bytes as number[])[0] === 255) throw new Error("invalid history"); return null;
        case "open_message": if (world.openError) throw world.openError; return world.openResult;
        case "epoch": return null;
        case "behind": if ((d!.group as ArrayLike<number> | undefined)?.length !== 32) throw new Error("behind names its group"); return world.behind;
        case "accept_chain": {
          const chain = d!.chain as { group: number[] }[];
          this.state.groups = [...this.state.groups.map(x => ({ ...x, live: false, members: [], record: { ...x.record, status: "superseded" as const } })),
            { ...summary("rejoin"), group: chain.at(-1)!.group, incarnation: String(chain.length - 1), record: { ...summary("rejoin").record, checkpoint: null } }];
          return chain.at(-1)!.group;
        }
        default: throw new Error(`unexpected fake agent op ${op}`);
      }
    }
    take_changes() { const c = emptyChanges(); c.groups = [{ key: [1], value: [...enc.encode(JSON.stringify(this.state))] }]; return c; }
    free() {}
  }
  const factory: AgentFactory = { create: () => new FakeAgent(structuredClone(initial)), load: c => new FakeAgent(JSON.parse(dec.decode(new Uint8Array(c.groups[0]!.value!)))) };
  const name = crypto.randomUUID(), databases = new IDBFactory();
  let storage = await openStorage(name, databases);
  const durable = async (): Promise<State> => JSON.parse(dec.decode(new Uint8Array((await storage.load()).groups[0]!.value!)));
  const world = {
    groups: initial.groups.length && hasRemoteGroup ? [{ conversation, group, incarnation: 0, epoch: 0, pending: false }] : [],
    head: 0, oldest: 0, revocations: [] as Revocation[], behind: false, joinError: false,
    chain: [] as { group: string; descriptor: string }[],
    openError: null as Error | null, openResult: { reason: "no_key" } as { reason: "no_key" | "cannot_open" } | { ok: number[] },
    claimed: [] as { device_key: string; package: string }[],
    onClaim: () => {}, onDevices: () => {},
    entries: [] as { epoch: number; commit: string; proposals: string[] }[],
    welcomes: [] as { id: string; group: string; welcome: string }[],
    chunks: [] as { id: string; group: string; sender_uid: string; chunk: string }[],
    wanted: [] as { group: string; device: string; assignment?: string }[], nothingToShare: false,
    commitError: null as RequestError | null, requestError: null as RequestError | null, shareError: false, ackError: false,
    beforeLog: null as null | (() => Promise<void>),
    /** Epochs the tree endpoint answers with, in order; then the log head. */
    treeEpochs: [] as number[],
    /** Replaces the tree endpoint's answer. */
    treeResponse: null as null | ((signal: AbortSignal) => Promise<Response>),
    packages: 5, devices: [] as { uid: string; device_key: string; hpke_key: string; created_at: number; signature: string }[],
  };
  let welcomeHandler = (_welcome: { channels: unknown[] }) => {};
  let eventHandler = (_event: Event) => {};
  let statusHandler = (_status: ConnectionStatus) => {};
  const link = {
    status: { state: "idle" }, isOnline: false, serverKey: server, endpoint: "ws://gwar.test/ws",
    onStatus: (fn: (status: ConnectionStatus) => void) => { statusHandler = fn; return () => {}; }, onEvent: (fn: (event: Event) => void) => { eventHandler = fn; return () => {}; }, onWelcome: (fn: (welcome: { channels: unknown[] }) => void) => { welcomeHandler = fn; return () => {}; },
    connect: async () => { link.status.state = "online"; link.isOnline = true; welcomeHandler({ channels: options.channels ?? [], server: { server_chat: { encrypted: options.serverChat ?? false, history: options.serverChat ?? false } }, file_access: "access" } as never); return {} as never; },
    close: () => { link.isOnline = false; },
    request: async (op: string, data: Record<string, unknown>) => {
      calls.push({ op, data });
      switch (op) {
        case "mls.device.publish": return {};
        case "mls.key_packages.publish": if ((data.packages as string[]).length) world.packages = 5; return { count: world.packages };
        case "mls.groups": return { groups: world.groups, next: null };
        case "mls.welcomes": return { welcomes: world.welcomes, next: null };
        case "mls.welcomes.ack": {
          // Only after the Welcome's durable write; one to a superseded group has nothing to write.
          const stored = await durable();
          expect(stored.welcomes.includes((data.ids as string[])[0]!) || stored.groups.some(g => g.record.status === "superseded")).toBe(true);
        } if (world.ackError) { world.ackError = false; throw new Error("lost ack"); } world.welcomes = []; return {};
        case "mls.log": await world.beforeLog?.(); return { head: world.head, oldest: world.oldest, entries: world.entries.filter(e => e.epoch >= Number(data.since)), pending: [], next: null };
        case "mls.group_info": return { group_info: b64(8), epoch: world.head };
        case "mls.commit": expect((await durable()).out?.commit).toEqual([4]); if (world.commitError) throw world.commitError; world.entries.push({ epoch: Number(data.epoch), commit: data.commit as string, proposals: [] }); world.head = Number(data.epoch) + 1; return { epoch: world.head };
        case "mls.key_packages.claim": world.onClaim(); return { packages: world.claimed, next: null };
        case "mls.devices": world.onDevices(); return { devices: world.devices, next: null };
        case "mls.history.fetch": return { chunks: world.chunks, next: null };
        case "mls.history.ack": expect((await durable()).imports).toBeGreaterThan(0); world.chunks = world.chunks.filter(c => !(data.ids as string[]).includes(c.id)); return {};
        case "mls.history.wanted": return { wanted: world.wanted, next: null };
        case "mls.history.request": if (world.requestError) throw world.requestError; return {};
        case "mls.group.create": world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }]; return { group };
        case "mls.history.decline": return {};
        case "mls.group_history": return { incarnations: world.chain, next: null };
        case "mls.history.share": expect((await storage.load()).outbox.length).toBeGreaterThan(0); if (world.shareError) throw new Error("lost share response"); return {};
        default: throw new Error(`unexpected request ${op}`);
      }
    },
  };
  const identity = { publicKey: device, device: undefined, sign: async () => b64(1, 64) } as unknown as Identity;
  const locks = { request: async (_name: string, _options: unknown, fn: () => Promise<void>) => fn() } as unknown as LockManager;
  const fetchTree = async (url: string | URL | Request, init?: RequestInit) => {
    calls.push({ op: "tree", data: { url: String(url), authorization: (init?.headers as Record<string, string>).Authorization } });
    if (world.treeResponse) return world.treeResponse(init!.signal!);
    return new Response(new Uint8Array([3, 3]), { headers: { "x-gwar-epoch": String(world.treeEpochs.shift() ?? world.head) } });
  };
  const connectOptions = { identity, seed: new Uint8Array(32), link: link as unknown as Link, revocations: () => world.revocations, locks, storage: async () => storage, factory, now: options.now, onError: (error: unknown) => errors.push(error), fetch: fetchTree as typeof fetch };
  const runtime = await MlsRuntime.connect(connectOptions);
  runtimes.push(runtime);
  // Initialization has completed, so changes below model subsequent reconnects/events.
  return { runtime, world, calls, operations, errors, durable, state: () => current.state, get storage() { return storage; },
    event: (event: Event) => eventHandler(event),
    proposal: () => eventHandler({ ev: "mls.proposal", d: { group, epoch: 0, proposal: "AQ" } }),
    commitEvent: () => eventHandler({ ev: "mls.commit", d: { group, epoch: 1 } } as Event),
    persist: () => storage.write(current.take_changes()),
    // A new page load: the same durable stores, fresh runtime memory.
    restart: async () => {
      await runtime.close(); link.status.state = "idle";
      storage = await openStorage(name, databases);
      const next = await MlsRuntime.connect(connectOptions);
      runtimes.push(next);
      return next;
    },
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
  it.each([{ pruned: false, failWrite: false }, { pruned: false, failWrite: true }, { pruned: true, failWrite: false }, { pruned: true, failWrite: true }])("imports an offline readmission before ack (%j)", async ({ pruned, failWrite }) => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.head = 3;
    f.world.entries = [{ epoch: 0, commit: "CQ", proposals: [] }, { epoch: 1, commit: "Yw", proposals: [] }, { epoch: 2, commit: "CQ", proposals: [] }];
    if (pruned) { f.world.oldest = 2; f.world.entries = f.world.entries.slice(2); }
    f.world.welcomes = [{ id: "readmission", group, welcome: "Aw" }];
    if (failWrite) {
      const write = f.storage.write.bind(f.storage);
      const spy = vi.spyOn(f.storage, "write").mockImplementation(async changes => {
        if (changes.groups.some(c => c.value && dec.decode(new Uint8Array(c.value)).includes('"readmission"'))) throw new Error("Welcome write aborted");
        await write(changes);
      });
      await expect(f.runtime.sync()).rejects.toThrow("Welcome write aborted");
      expect((await f.durable()).groups[0]!.record.status).toBe(pruned ? "active" : "removed");
      expect(f.calls.some(c => c.op === "mls.welcomes.ack")).toBe(false);
      spy.mockRestore();
    }
    await f.runtime.sync();
    expect((await f.durable()).groups[0]!.record.checkpoint!.epoch).toBe("3");
    expect((await f.durable()).welcomes).toEqual(["readmission"]);
    expect(f.operations.some(o => o.op === "replace_stale_welcome")).toBe(pruned);
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
    expect(f.calls.filter(c => c.op === "mls.welcomes.ack")).toHaveLength(1);
  });
  it("deduplicates a pruned-log replacement after a lost ack", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.head = 3; f.world.oldest = 3;
    f.world.welcomes = [{ id: "readmission", group, welcome: "Aw" }]; f.world.ackError = true;
    await expect(f.runtime.sync()).rejects.toThrow("lost ack");
    expect((await f.durable()).groups[0]!.record.checkpoint!.epoch).toBe("3");
    await f.runtime.sync();
    expect(f.operations.filter(o => o.op === "replace_stale_welcome")).toHaveLength(1);
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
    expect(f.world.welcomes).toEqual([]);
  });
  it.each([false, true])("externally rejoins a pruned log only without a usable Welcome (unusable: %s)", async unusable => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.head = 3; f.world.oldest = 3;
    if (unusable) f.world.welcomes = [{ id: "stale", group, welcome: "AA" }];
    await f.runtime.sync();
    expect(f.operations.some(o => o.op === "join_external")).toBe(true);
    expect(f.calls.some(c => c.op === "mls.welcomes.ack")).toBe(false);
    expect((await f.durable()).groups[0]!.record.checkpoint!.epoch).toBe("4");
  });
  it("removes a locally revoked leaf without admitting its advertised package again", async () => {
    const local = summary(); local.members.push({ device_key: peer, uid: await uidForPublicKey(b64(8)) });
    const f = await fixture({ ...clean(), groups: [local] });
    f.world.revocations = [{ account_key: b64(8), device_key: peer, revoked_at: 1 }];
    f.world.devices = [{ uid: await uidForPublicKey(b64(8)), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.claimed = [{ device_key: peer, package: "AQ" }];
    await f.runtime.sync(); await f.runtime.sync();
    expect(f.runtime.status(conversation)).toBe("ready");
    expect(f.calls.some(c => c.op === "mls.key_packages.claim")).toBe(false);
    expect(f.operations.filter(o => o.op === "commit").map(o => o.adds)).toEqual([[]]);
  });
  it("filters claimed adds again when revocations advance during the request", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.devices = [{ uid: await uidForPublicKey(b64(8)), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.claimed = [{ device_key: peer, package: "AQ" }, { device_key: b64(10), package: "Ag" }];
    f.world.onClaim = () => { f.world.revocations = [{ account_key: b64(8), device_key: peer, revoked_at: 1 }]; };
    await f.runtime.sync();
    expect(f.calls.some(c => c.op === "mls.key_packages.claim")).toBe(true);
    expect(f.operations.some(o => o.op === "commit")).toBe(false);
    expect(f.runtime.status(conversation)).toBe("ready");
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
  it("leaves a history job for the next pass when the group changes before sharing", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.head = 1; f.world.entries = [{ epoch: 0, commit: "CQ", proposals: [] }];
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    // Between the job's sync check and its share request (devices are also read while syncing).
    f.world.onDevices = () => { if (f.calls.at(-2)?.op === "mls.history.wanted") { f.world.onDevices = () => {}; f.commitEvent(); } };
    await f.runtime.sync();
    expect(f.calls.some(c => c.op === "mls.history.share")).toBe(false);
    await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.share").map(c => c.data.chunk)).toEqual(["Fg", "Fw"]);
  });
  it("atomically replaces history for a newer verified HPKE publication and keeps retries stable", async () => {
    const local = summary(); local.members.push({ device_key: peer, uid: b64(5,20) });
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [local] }, true, { now: () => now });
    const recipient = { uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) };
    f.world.devices = [recipient]; f.world.wanted = [{ group, device: peer }]; f.world.shareError = true;
    const jobs = async () => (await f.storage.load()).outbox.map(c => JSON.parse(dec.decode(new Uint8Array(c.value!))));
    await expect(f.runtime.sync()).rejects.toThrow("lost share response");
    const original = await jobs();
    f.world.devices = [{ ...recipient, created_at: 2, hpke_key: b64(8), signature: "invalid" }];
    // A bad publication backs this job off without failing the pass.
    await f.runtime.sync();
    expect(String(f.errors.at(-1))).toContain("invalid HPKE signature");
    expect(await jobs()).toEqual(original);
    f.world.devices[0]!.signature = b64(8,64);
    await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.share")).toHaveLength(1);
    now += 5_000;
    const write = f.storage.write.bind(f.storage);
    const spy = vi.spyOn(f.storage, "write").mockImplementation(async changes => {
      if (changes.outbox.some(c => c.value && dec.decode(new Uint8Array(c.value)).includes('"created_at":2'))) throw new Error("replacement write aborted");
      await write(changes);
    });
    await expect(f.runtime.sync()).rejects.toThrow("replacement write aborted");
    expect(await jobs()).toEqual(original);
    expect(f.calls.filter(c => c.op === "mls.history.share")).toHaveLength(1);
    spy.mockRestore();
    await expect(f.runtime.sync()).rejects.toThrow("lost share response");
    const replacement = await jobs();
    expect(replacement[0].publication).toMatchObject({ created_at: 2, hpke_key: b64(8) });
    expect(replacement[0].chunks).not.toEqual(original[0].chunks);
    // An older advertisement must not roll the job back; retries reuse bytes.
    f.world.devices = [recipient]; f.world.shareError = false;
    await f.runtime.sync();
    expect(f.state().shares).toBe(2);
    expect(f.calls.filter(c => c.op === "mls.history.share").map(c => c.data.chunk)).toEqual(["Fg", "Fw", "Fw", "GA"]);
  }, 20_000);
  it("backs off a recipient whose history cannot be prepared and still shares with the others", async () => {
    const other = b64(4), local = summary();
    local.members.push({ device_key: peer, uid: b64(5,20) }, { device_key: other, uid: b64(6,20) });
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [local] }, true, { now: () => now });
    f.world.devices = [
      { uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: "invalid" },
      { uid: b64(6,20), device_key: other, hpke_key: b64(7), created_at: 1, signature: b64(7,64) },
    ];
    f.world.wanted = [{ group, device: peer }, { group, device: other }];
    const shared = () => f.calls.filter(c => c.op === "mls.history.share").map(c => c.data.recipient);
    await f.runtime.sync();
    expect(String(f.errors.at(-1))).toContain("invalid HPKE signature");
    expect(shared()).toEqual([other, other]);
    // Not retried before its backoff, then shared once the publication is valid.
    f.world.wanted = []; f.world.devices[0]!.signature = b64(8,64);
    await f.runtime.sync();
    expect(shared()).toEqual([other, other]);
    now += 5_000; await f.runtime.sync();
    expect(shared()).toEqual([other, other, peer, peer]);
  });
  it("opens through queries without writes and distinguishes operational failures", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    const message = { conversation, group, epoch: 0, message_id: b64(2,16), revision: 0, ciphertext: "AQ" };
    const write = vi.spyOn(f.storage, "write").mockRejectedValue(new Error("disk unavailable"));
    f.world.openResult = { reason: "cannot_open" };
    expect(await f.runtime.openMessage(message)).toEqual({ reason: "cannot_open" });
    expect(await f.runtime.openMessage({ ...message, revision: -1 })).toEqual({ reason: "cannot_open" });
    expect(await f.runtime.openMessage({ ...message, group: "bad" })).toEqual({ reason: "cannot_open" });
    f.world.openResult = { reason: "no_key" };
    expect(await f.runtime.openMessage(message)).toEqual({ reason: "no_key" });
    expect(write).not.toHaveBeenCalled();
    f.world.openError = new Error("wasm runtime failed");
    await expect(f.runtime.openMessage(message)).rejects.toThrow("wasm runtime failed");
    const load = vi.spyOn(f.storage, "load").mockRejectedValue(new Error("database unavailable"));
    await expect(f.runtime.openMessage(message)).rejects.toThrow("database unavailable");
    load.mockRestore(); write.mockRestore();
    await f.runtime.close();
    await expect(f.runtime.openMessage(message)).rejects.toThrow("runtime closed");
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
  it("does not immediately resynchronize again for the same missing epoch", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    const timer = vi.spyOn(globalThis, "setTimeout");
    try {
      const message = { conversation, group, epoch: 0, message_id: b64(2,16), revision: 0, ciphertext: "AQ" };
      await f.runtime.openMessage(message); await f.runtime.openMessage(message);
      expect(timer.mock.calls.filter(([, delay]) => delay === 0)).toHaveLength(1);
    } finally { timer.mockRestore(); }
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
describe("Restarts", () => {
  const restartedGroup = b64(12);
  const statement = (op: Record<string, unknown> | undefined) => dec.decode(new Uint8Array(op!.descriptor as number[]));
  // A direct message of this device's account, which it may restart.
  const asParticipant = async () => { conversation = `dm:${[await uidForPublicKey(device), b64(5, 20)].sort().join(":")}`; };
  it("restarts only after two checks a minute apart find the server behind the live pin", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("active", "5")] }, true, { now: () => now });
    f.world.head = 3;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 59_000;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    expect(f.operations.some(o => o.op === "create")).toBe(false);
    now += 1_000;
    // The server holds the new group from its first epoch.
    f.world.beforeLog = async () => { if (f.operations.some(o => o.op === "create")) f.world.head = 0; };
    // Restarting is not a failure of the pass.
    await f.runtime.sync();
    const create = f.operations.find(o => o.op === "create");
    // The next incarnation, naming the pinned group.
    expect(statement(create).split("\n").slice(4, 6)).toEqual(["1", group]);
    expect(f.calls.some(c => c.op === "mls.group.create")).toBe(true);
  });
  it("starts the wait again after a verified GroupInfo at the checkpoint, even when joining then fails", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("rejoin", "5")] }, true, { now: () => now });
    f.state().groups = [summary("rejoin", "5")]; f.state().out = null; await f.persist(); f.world.behind = true;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 30_000; f.world.behind = false; f.world.joinError = true;
    await expect(f.runtime.sync()).rejects.toThrow("join failed");
    now += 30_000; f.world.behind = true; f.world.joinError = false;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    expect(f.operations.some(o => o.op === "create")).toBe(false);
    now += 60_000;
    await f.runtime.sync().catch(() => {});
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it("does not let a log that only reports its head at the checkpoint cancel the wait", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("active", "5")] }, true, { now: () => now });
    f.world.head = 3;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 30_000; f.world.head = 5; f.commitEvent();
    await f.runtime.sync();
    now += 30_000; f.world.head = 3; f.commitEvent();
    await f.runtime.sync().catch(() => {});
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it("discards a pending check when the checkpoint changes", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("active", "5")] }, true, { now: () => now });
    f.world.head = 3;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    f.state().groups[0]!.record.checkpoint!.hash = [1]; await f.persist();
    now += 60_000;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    expect(f.operations.some(o => o.op === "create")).toBe(false);
    now += 60_000;
    await f.runtime.sync().catch(() => {});
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it("rejoins a direct message from its GroupInfo alone", async () => {
    await asParticipant();
    const f = await fixture({ ...clean(), groups: [summary("rejoin", "5")] });
    f.state().groups = [summary("rejoin", "5")]; f.state().out = null; await f.persist(); f.world.head = 5;
    await f.runtime.sync().catch(() => {});
    expect(f.calls.some(c => c.op === "tree")).toBe(false);
    expect(f.operations.find(o => o.op === "behind")?.tree).toBeNull();
    expect(f.operations.find(o => o.op === "join_external")?.tree).toBeNull();
  });
  it("counts a GroupInfo behind the checkpoint when rejoining", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("rejoin", "5")] }, true, { now: () => now });
    f.state().groups = [summary("rejoin", "5")]; f.state().out = null; await f.persist(); f.world.behind = true;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 60_000;
    await f.runtime.sync().catch(() => {});
    expect(f.operations.filter(o => o.op === "join_external")).toHaveLength(1); // only the one before the server fell behind
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it.each(["dm", "server"])("accepts the server's restart from the live pin and joins it (%s)", async kind => {
    if (kind === "server") conversation = "server";
    const f = await fixture({ ...clean(), groups: [summary()] }, true, { serverChat: kind === "server" });
    f.world.groups = [{ conversation, group: restartedGroup, incarnation: 1, epoch: 0, pending: false }];
    f.world.chain = [{ group, descriptor: "AQ" }, { group: restartedGroup, descriptor: "Ag" }];
    await f.runtime.sync();
    expect(f.calls.find(c => c.op === "mls.group_history")?.data).toEqual({ conversation, after: null });
    expect(f.operations.find(o => o.op === "accept_chain")).toMatchObject({ conversation, chain: [{ group: Array(32).fill(2), descriptor: [1] }, { group: Array(32).fill(12), descriptor: [2] }] });
    expect(f.calls.find(c => c.op === "mls.group_info")?.data).toEqual({ group: restartedGroup });
    const stored = (await f.durable()).groups.map(g => [g.group[0], g.incarnation, g.live, g.record.status]);
    expect(stored).toEqual([[2, "0", false, "superseded"], [12, "1", true, "active"]]);
    expect(f.runtime.status(conversation)).toBe("ready");
    expect(f.runtime.roster(conversation)).toMatchObject({ group: restartedGroup, restarted: true });
  });
  it("names the restarted group while it cannot be joined yet", async () => {
    const f = await fixture({ ...clean(), groups: [summary()] });
    f.world.groups = [{ conversation, group: restartedGroup, incarnation: 1, epoch: 0, pending: false }];
    f.world.chain = [{ group, descriptor: "AQ" }, { group: restartedGroup, descriptor: "Ag" }];
    f.world.joinError = true;
    await expect(f.runtime.sync()).rejects.toThrow("join failed");
    await vi.waitFor(() => expect(f.runtime.roster(conversation)).toMatchObject({ group: restartedGroup, restarted: true }));
    expect(f.runtime.status(conversation)).toBe("securing");
  });
  it("anchors the chain request at a restarted pin", async () => {
    const pin = { ...summary(), incarnation: "2" };
    const f = await fixture({ ...clean(), groups: [pin] }, false);
    f.world.groups = [{ conversation, group: restartedGroup, incarnation: 3, epoch: 0, pending: false }];
    await f.runtime.sync().catch(() => {});
    expect(f.calls.find(c => c.op === "mls.group_history")?.data).toEqual({ conversation, after: encodeBase64Url(enc.encode("1")) });
  });
  it.each([0, 1])("stays blocked when the server holds incarnation %i below or beside the live pin", async incarnation => {
    let now = 1_000_000;
    const pin = { ...summary("active", "5"), incarnation: "1" };
    pin.members.push({ device_key: peer, uid: b64(5,20) });
    const f = await fixture({ ...clean(), groups: [pin] }, false, { now: () => now });
    f.world.groups = [{ conversation, group: restartedGroup, incarnation, epoch: 0, pending: false }];
    // Neither history for a peer nor missing keys go out while it is held.
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.wanted = [{ group, device: peer }];
    f.world.welcomes = [{ id: "readmission", group, welcome: "AQ" }];
    await f.runtime.openMessage({ conversation, group, epoch: 4, message_id: b64(2,16), revision: 0, ciphertext: "AQ" });
    for (let i = 0; i < 3; i++) { await f.runtime.sync(); now += 60_000; }
    expect(f.calls.some(c => ["mls.log", "mls.group_history", "mls.group_info", "mls.group.create", "mls.history.share", "mls.history.request", "mls.welcomes.ack"].includes(c.op))).toBe(false);
    expect(f.operations.some(o => o.op === "welcome")).toBe(false);
    expect(f.runtime.status(conversation)).toBe("securing");
    await expect(f.runtime.sealMessage(conversation, async () => ({}) as Plaintext)).rejects.toThrow();
  });
  it("shares no queued history once a synchronized conversation is held", async () => {
    const pin = { ...summary(), incarnation: "1" };
    pin.members.push({ device_key: peer, uid: b64(5,20) });
    const f = await fixture({ ...clean(), groups: [pin] });
    await f.runtime.sync();
    f.world.groups = [{ conversation, group: restartedGroup, incarnation: 1, epoch: 0, pending: false }];
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.wanted = [{ group, device: peer }];
    await f.runtime.sync();
    expect([...(await f.storage.load()).outbox].some(c => dec.decode(new Uint8Array(c.key)).startsWith("@runtime/history/"))).toBe(true);
    expect(f.calls.some(c => c.op === "mls.history.share")).toBe(false);
    f.world.groups = [{ conversation, group, incarnation: 1, epoch: 0, pending: false }];
    await f.runtime.sync();
    expect(f.calls.some(c => c.op === "mls.history.share")).toBe(true);
  });
  it("keeps the trigger when a pending join loses to an entry it did not validate", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("joining", "5")], out: { ...pending(), epoch: "5" } }, true, { now: () => now });
    f.world.head = 3;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 60_000; f.world.head = 6; f.world.behind = true;
    f.world.entries = [{ epoch: 5, commit: "BQ", proposals: [] }];
    await f.runtime.sync().catch(() => {});
    expect(f.operations.filter(o => o.op === "reconcile")).toHaveLength(1);
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it("starts the wait again when a pending commit loses to a validated removal", async () => {
    await asParticipant();
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("active", "5")], out: { ...pending(), epoch: "5" } }, true, { now: () => now });
    f.world.head = 3;
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    now += 60_000; f.world.head = 6; f.world.behind = true;
    f.world.entries = [{ epoch: 5, commit: "Yw", proposals: [] }];
    await expect(f.runtime.sync()).rejects.toThrow("behind");
    expect(f.operations.some(o => o.op === "create")).toBe(false);
    now += 60_000;
    await f.runtime.sync().catch(() => {});
    expect(f.operations.some(o => o.op === "create")).toBe(true);
  });
  it("acknowledges a Welcome to a superseded group without importing it", async () => {
    const f = await fixture({ ...clean(), groups: [{ ...summary("superseded"), live: false }] }, false);
    f.world.welcomes = [{ id: "old", group, welcome: "AQ" }];
    await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.welcomes.ack").map(c => c.data.ids)).toEqual([["old"]]);
    expect(f.operations.some(o => o.op === "welcome")).toBe(false);
  });
});
describe("Public groups", () => {
  const channel = (history = true) => ({ id: 1, encrypted: true, history });
  const publicOptions = (target: string, history = true) => target === "server" ? { serverChat: true, channels: [] } : { channels: [channel(history)] };
  const asChannel = () => { conversation = "channel:1"; };
  it.each(["channel:1", "server"])("joins an encrypted channel's group and then asks for its history (%s)", async target => {
    conversation = target;
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { ...publicOptions(target) });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    await f.runtime.sync(); random.mockRestore();
    expect(f.operations.some(o => o.op === "join_external")).toBe(true);
    expect(f.calls.filter(c => c.op === "mls.history.request")).toEqual([{ op: "mls.history.request", data: { group } }]);
    await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(1);
  });
  it.each(["channel:1", "server"])("joins with the ratchet tree of the GroupInfo's epoch, fetched over HTTP (%s)", async target => {
    conversation = target;
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { ...publicOptions(target) });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    f.world.head = 4; f.world.treeEpochs = [3];
    await f.runtime.sync(); random.mockRestore();
    // The first tree was of an older epoch: both are asked for again.
    expect(f.calls.filter(c => c.op === "mls.group_info")).toHaveLength(2);
    expect(f.calls.filter(c => c.op === "tree").map(c => c.data)).toEqual(Array(2).fill({ url: `http://gwar.test/api/mls/${group}/tree`, authorization: "Bearer access" }));
    expect(f.operations.find(o => o.op === "join_external")?.tree).toEqual([3, 3]);
  });
  it("stops reading a tree past the size limit", async () => {
    asChannel();
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { channels: [channel()] });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    let sent = 0, cancelled = false;
    // No content length: the body streams 1 MiB chunks without end.
    f.world.treeResponse = async () => new Response(new ReadableStream({
      pull: controller => { sent++; controller.enqueue(new Uint8Array(1024 * 1024)); },
      cancel: () => { cancelled = true; },
    }), { headers: { "x-gwar-epoch": "0" } });
    await f.runtime.sync(); random.mockRestore();
    expect(String(f.errors.at(-1))).toContain("MLS tree too large");
    expect(cancelled).toBe(true);
    expect(sent).toBeLessThanOrEqual(10);
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
  });
  it.each([
    ["a failed status", { status: 503, headers: { "x-gwar-epoch": "0" } }, "(503)"],
    ["no epoch", { headers: {} }, "MLS tree epoch"],
  ])("ends a refused tree response with %s without reading it", async (_, init, message) => {
    asChannel();
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { channels: [channel()] });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    const signals: AbortSignal[] = [];
    f.world.treeResponse = async signal => { signals.push(signal); return new Response(new ReadableStream({ pull: () => new Promise(() => {}) }), init); };
    await f.runtime.sync(); random.mockRestore();
    expect(String(f.errors.at(-1))).toContain(message);
    expect(signals.length).toBeGreaterThan(0);
    expect(signals.every(signal => signal.aborted)).toBe(true);
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
  });
  it("ends a stalled tree download when the connection drops", async () => {
    asChannel();
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { channels: [channel()] });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    let started!: () => void;
    const fetching = new Promise<void>(resolve => { started = resolve; });
    f.world.treeResponse = signal => new Promise((_, reject) => { started(); signal.addEventListener("abort", () => reject(signal.reason)); });
    const pass = f.runtime.sync();
    await fetching; random.mockRestore();
    f.reconnect();
    await pass.catch(() => {});
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
  });
  it("gives up a join while the group keeps changing", async () => {
    asChannel();
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { channels: [channel()] });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    f.world.head = 4; f.world.treeEpochs = [1, 2, 3];
    await f.runtime.sync(); random.mockRestore();
    expect(String(f.errors.at(-1))).toContain("changed while joining");
    expect(f.operations.some(o => o.op === "join_external")).toBe(false);
  });
  it.each(["channel:1", "server"])("keeps the history request intent durably until a request succeeds (%s)", async target => {
    conversation = target;
    let now = 1_000_000;
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { ...publicOptions(target), now: () => now });
    const intents = async () => (await f.storage.load()).outbox.map(c => dec.decode(new Uint8Array(c.key))).filter(k => k.startsWith("@runtime/request/"));
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    f.world.requestError = new RequestError("rate_limited", "rate_limited");
    await f.runtime.sync(); random.mockRestore();
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(1);
    expect(await intents()).toHaveLength(1);
    // Asked again only after the server's one-minute limit.
    f.world.requestError = null;
    now += 59_999; await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(1);
    now += 1; await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(2);
    expect(await intents()).toEqual([]);
  });
  it.each(["channel:1", "server"])("does not back off a channel this device restarted (%s)", async target => {
    conversation = target;
    let now = 1_000_000;
    const f = await fixture({ ...clean(), groups: [summary("active", "5")] }, true, { ...publicOptions(target), now: () => now });
    f.world.head = 3;
    await f.runtime.sync();
    expect(String(f.errors.at(-1))).toContain("behind");
    now += 60_000;
    f.world.beforeLog = async () => { if (f.operations.some(o => o.op === "create")) f.world.head = 0; };
    const errors = f.errors.length;
    await f.runtime.sync();
    expect(f.calls.some(c => c.op === "mls.group.create")).toBe(true);
    expect(f.errors).toHaveLength(errors);
    // The new group's next change is synchronized right away.
    const logs = f.calls.filter(c => c.op === "mls.log").length;
    f.commitEvent(); await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.log").length).toBeGreaterThan(logs);
  });
  it("does not back off a channel restarted while sending", async () => {
    asChannel();
    let now = 1_000_000;
    const random = vi.spyOn(Math, "random").mockReturnValue(1);
    const f = await fixture({ ...clean(), groups: [summary("active", "5")] }, true, { channels: [channel()], now: () => now });
    f.world.head = 3;
    // Three failed passes: the next one is due 70 s after the first.
    await f.runtime.sync(); now += 10_000; await f.runtime.sync(); now += 20_000; await f.runtime.sync();
    random.mockRestore();
    now += 30_000;
    f.world.beforeLog = async () => { if (f.operations.some(o => o.op === "create")) f.world.head = 0; };
    await expect(f.runtime.sealMessage(conversation, async () => { throw new Error("not reached"); })).rejects.toThrow("restarted");
    expect(f.calls.some(c => c.op === "mls.group.create")).toBe(true);
    const logs = f.calls.filter(c => c.op === "mls.log").length;
    f.commitEvent(); await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.log").length).toBeGreaterThan(logs);
  });
  it.each(["channel:1", "server"])("asks for history after a restart when the request before it failed (%s)", async target => {
    conversation = target;
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { ...publicOptions(target) });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    f.world.requestError = new RequestError("unavailable", "unavailable");
    await f.runtime.sync(); random.mockRestore();
    expect(String(f.errors.at(-1))).toContain("unavailable");
    expect(f.operations.filter(o => o.op === "join_external")).toHaveLength(1);
    f.world.requestError = null;
    const runtime = await f.restart();
    await runtime.sync();
    expect(f.operations.filter(o => o.op === "join_external")).toHaveLength(1);
    expect(f.calls.filter(c => c.op === "mls.history.request")).toHaveLength(2);
    expect((await f.storage.load()).outbox.map(c => dec.decode(new Uint8Array(c.key))).filter(k => k.startsWith("@runtime/request/"))).toEqual([]);
  });
  it.each(["channel:1", "server"])("ignores plaintext channels (%s)", async target => {
    conversation = target;
    const random = vi.spyOn(Math, "random").mockReturnValue(0);
    const f = await fixture(clean(), true, { serverChat: false, channels: [{ ...channel(), encrypted: false }] });
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 0, pending: false }];
    await f.runtime.sync(); random.mockRestore();
    expect(f.operations.some(o => o.op === "join_external" || o.op === "create")).toBe(false);
  });
  it.each(["channel:1", "server"])("creates a missing channel group only after a random delay (%s)", async target => {
    conversation = target;
    let now = 1_000_000;
    const random = vi.spyOn(Math, "random").mockReturnValue(0.5);
    try {
      const f = await fixture(clean(), false, { ...publicOptions(target, false), now: () => now });
      expect(f.operations.some(o => o.op === "create")).toBe(false);
      now += 2_499; await f.runtime.sync();
      expect(f.operations.some(o => o.op === "create")).toBe(false);
      now += 1; await f.runtime.sync();
      expect(f.calls.some(c => c.op === "mls.group.create")).toBe(true);
      expect(f.operations.find(o => o.op === "settle")).toMatchObject({ winner: Array(32).fill(2) });
    } finally { random.mockRestore(); }
  });
  it("creates its own new channel's group right away", async () => {
    asChannel();
    const f = await fixture(clean(), false, { channels: [] });
    f.event({ ev: "channel.created", d: { ...channel(), parent: null, name: "Sealed", topic: "", position: 0, has_password: false, max_clients: null } } as Event);
    f.runtime.channelCreated(1);
    await f.runtime.sync();
    expect(f.calls.some(c => c.op === "mls.group.create")).toBe(true);
  });
  it("forgets the group of a deleted channel", async () => {
    asChannel();
    const f = await fixture({ ...clean(), groups: [summary()] }, true, { channels: [channel()] });
    expect(f.operations.some(o => o.op === "forget")).toBe(false);
    f.event({ ev: "channel.deleted", d: { channel: 1 } } as Event);
    await f.runtime.sync();
    expect(f.operations.find(o => o.op === "forget")).toMatchObject({ group: Array(32).fill(2) });
    expect((await f.durable()).groups).toEqual([]);
  });
  it("forgets a deleted channel's history jobs with its group", async () => {
    asChannel();
    const local = summary(); local.members.push({ device_key: peer, uid: b64(5,20) });
    const f = await fixture({ ...clean(), groups: [local] }, true, { channels: [channel()] });
    const jobs = async () => (await f.storage.load()).outbox.map(c => dec.decode(new Uint8Array(c.key))).filter(k => k.startsWith("@runtime/history/"));
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.wanted = [{ group, device: peer, assignment: b64(1,16) }]; f.world.shareError = true;
    await expect(f.runtime.sync()).rejects.toThrow("lost share response");
    expect(await jobs()).toHaveLength(1);
    f.world.wanted = [];
    f.event({ ev: "channel.deleted", d: { channel: 1 } } as Event);
    await f.runtime.sync();
    expect(await jobs()).toEqual([]);
  });
  it.each(["channel:1", "server"])("answers an assignment with its chunks, or declines it with nothing to share (%s)", async target => {
    conversation = target;
    const local = summary(); local.members.push({ device_key: peer, uid: b64(5,20) });
    const f = await fixture({ ...clean(), groups: [local] }, true, { ...publicOptions(target) });
    f.world.devices = [{ uid: b64(5,20), device_key: peer, hpke_key: b64(6), created_at: 1, signature: b64(7,64) }];
    f.world.wanted = [{ group, device: peer, assignment: b64(1,16) }];
    await f.runtime.sync();
    const shares = f.calls.filter(c => c.op === "mls.history.share");
    expect(shares.length).toBe(2);
    expect(shares.every(c => c.data.assignment === b64(1,16))).toBe(true);
    expect(f.calls.find(c => c.op === "mls.devices")?.data.uids).toEqual([b64(5,20)]);
    f.world.nothingToShare = true; f.world.wanted = [{ group, device: peer, assignment: b64(2,16) }];
    await f.runtime.sync();
    expect(f.calls.filter(c => c.op === "mls.history.decline")).toEqual([{ op: "mls.history.decline", data: { group, device: peer, assignment: b64(2,16) } }]);
    expect(f.calls.filter(c => c.op === "mls.history.share")).toHaveLength(2);
  });
  it.each(["channel:1", "server"])("updates its own leaf after seven days without an own commit, one group at a time (%s)", async target => {
    conversation = target;
    const now = 100 * 86400_000;
    const local = summary(); local.record.own_commit_at = now - 7 * 86400_000 + 1;
    const f = await fixture({ ...clean(), groups: [local] }, true, { ...publicOptions(target), now: () => now });
    expect(f.operations.some(o => o.op === "commit")).toBe(false);
    f.state().groups[0]!.record.own_commit_at = now - 7 * 86400_000 - 1; await f.persist();
    await f.runtime.sync();
    expect(f.operations.filter(o => o.op === "commit")).toMatchObject([{ adds: [], removes: [] }]);
    expect(f.calls.some(c => c.op === "mls.commit")).toBe(true);
  });
  it("does not share after an admission in a channel group", async () => {
    asChannel();
    const f = await fixture({ ...clean(), groups: [summary()] }, true, { channels: [channel()] });
    f.world.entries = [{ epoch: 0, commit: "AQ", proposals: [] }]; f.world.head = 1;
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 1, pending: false }];
    await f.runtime.sync();
    expect(f.state().groups[0]!.members.length).toBe(2);
    expect(f.calls.some(c => c.op === "mls.history.share" || c.op === "mls.key_packages.claim")).toBe(false);
  });
  it("does not read the log of a synchronized channel group that has not changed", async () => {
    asChannel();
    const f = await fixture({ ...clean(), groups: [summary()] }, true, { channels: [channel()] });
    const logs = () => f.calls.filter(c => c.op === "mls.log").length;
    const before = logs();
    await f.runtime.sync();
    expect(logs()).toBe(before);
    f.commitEvent(); await f.runtime.sync();
    expect(logs()).toBe(before + 1);
    f.world.groups = [{ conversation, group, incarnation: 0, epoch: 1, pending: false }];
    await f.runtime.sync();
    expect(logs()).toBe(before + 2);
  });
});
