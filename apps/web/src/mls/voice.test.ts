import { createHash } from "node:crypto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import { RequestError } from "../net/connection";
import type { Identity } from "../net/identity";
import { SframeState, type Control } from "../voice/sframe/state";
import { Engine } from "./engine";
import { memoryStorage } from "./storage";
import type { AgentFactory, Changes, GroupSummary, WasmAgent } from "./types";
import { ACK_WAIT_MS, VoiceCall, type VoiceHost } from "./voice";

const b64 = (n: number, len = 32) => encodeBase64Url(new Uint8Array(len).fill(n));
const SERVER = b64(1), PARENT = b64(2), CALL = b64(3, 16), PART = b64(4, 16);
const DEVICES = { self: b64(10), bob: b64(11), carol: b64(12) };
type Member = GroupSummary["members"][number];
const member = (leaf: number, who: keyof typeof DEVICES): Member => ({ leaf, device_key: DEVICES[who], uid: `${who}-uid` });
const enc = (value: unknown) => [...new TextEncoder().encode(JSON.stringify(value))];
const dec = <T,>(bytes: number[]): T => JSON.parse(new TextDecoder().decode(new Uint8Array(bytes))) as T;

/** The server's view of one call: its group, log and pending removals. */
class FakeServer {
  group: string | null = null;
  members: Member[] = [];
  log: { epoch: number; commit: string; proposals: string[] }[] = [];
  pending: string[] = [];
  acks: number[] = [];
  repeats = 0;
  requests: string[] = [];
  get head() { return this.log.length; }
  /** Another device's commit: the new member list. */
  commit(members: Member[]) { this.log.push({ epoch: this.head, commit: encodeBase64Url(new Uint8Array(enc({ members }))), proposals: [] }); this.members = members; }
}

/** The voice agent's observable state, driven by the fake server's commit bytes. */
class FakeVoiceAgent implements WasmAgent {
  status: GroupSummary["record"]["status"] | null = null;
  group: number[] = [];
  candidate: number[] | null = null;
  epoch = 0;
  members: Member[] = [];
  out: { epoch: string; hash: number[]; commit: number[]; group_info: number[]; welcome: null } | null = null;
  blocked = false;
  parentMissing: number[] = [];
  serverPending = false;
  constructor(world: { agent?: FakeVoiceAgent }) { world.agent = this; }
  private summary(): GroupSummary[] {
    if (!this.status) return [];
    return [{ group: this.group, conversation: "voice:7", incarnation: "0", live: this.status !== "creating", history: false, record: { status: this.status, checkpoint: this.status === "creating" ? null : { epoch: String(this.epoch), hash: [] }, server_pending: this.serverPending }, members: structuredClone(this.members) }];
  }
  execute(request: unknown): unknown {
    const [op, d] = Object.entries(request as Record<string, Record<string, unknown>>)[0]!;
    switch (op) {
      case "inventory": return this.summary();
      case "gate": return { status: this.status, blocked: [], server_pending: this.serverPending, rejoin: false, pending_commit: !!this.out };
      case "outbox": return this.out;
      case "create": {
        const bytes = d!.descriptor as number[];
        const text = new TextDecoder().decode(new Uint8Array(bytes));
        const length = Number(text.slice(0, text.indexOf(":")));
        const statement = new TextEncoder().encode(text.slice(text.indexOf(":") + 1)).slice(0, length);
        this.candidate = [...createHash("sha256").update(statement).digest()];
        this.status = "creating"; this.group = this.candidate;
        return enc({ create: encodeBase64Url(new Uint8Array(this.candidate)) });
      }
      case "settle": {
        if ((d!.winner as number[]).join() === this.candidate!.join()) { this.status = "active"; this.epoch = 0; this.members = [member(0, "self")]; }
        else { this.status = null; }
        return true;
      }
      case "join_external": {
        const info = dec<{ epoch: number; members: Member[]; group: string }>(d!.bytes as number[]);
        this.group = [...decodeBase64Url(info.group)];
        const members = [...info.members, member(Math.max(-1, ...info.members.map(m => m.leaf)) + 1, "self")];
        this.status = "joining"; this.epoch = info.epoch; this.members = info.members;
        this.out = { epoch: String(info.epoch), hash: [7, info.epoch], commit: enc({ members }), group_info: [1], welcome: null };
        return { ...this.out, group: this.group };
      }
      case "reconcile": {
        const result = d!.result as { case: string; commit?: number[] };
        if (result.case === "head") return "retry";
        if (result.case === "entry" && result.commit!.join() === this.out?.commit.join()) {
          this.members = dec<{ members: Member[] }>(this.out.commit).members; this.epoch++; this.status = "active"; this.out = null;
          return "accepted";
        }
        this.out = null; return "rebuild";
      }
      case "receive": this.members = dec<{ members: Member[] }>(d!.commit as number[]).members; this.epoch++; return null;
      case "commit": {
        const removes = d!.removes as number[];
        const members = this.members.filter(m => !removes.includes(m.leaf));
        this.parentMissing = this.parentMissing.filter(leaf => !removes.includes(leaf));
        this.serverPending = false;
        this.out = { epoch: String(this.epoch), hash: [8, this.epoch], commit: enc({ members, by: "self" }), group_info: [1], welcome: null };
        return this.out;
      }
      case "voice_pending": this.serverPending = (d!.proposals as unknown[]).length > 0; return null;
      default: throw new Error(`unexpected ${op}`);
    }
  }
  execute_with_parent(request: unknown): unknown {
    const [op] = Object.keys(request as object);
    if (op === "voice_parent_removals") return this.parentMissing;
    if (this.blocked || this.parentMissing.length) throw new Error("group cannot send");
    return { epoch: String(this.epoch), own_leaf: this.members.find(m => m.device_key === DEVICES.self)!.leaf, members: this.members.map(m => ({ leaf: m.leaf, device: m.device_key, key: Array(32).fill(m.leaf + this.epoch * 10 + 1) })), send_until: null };
  }
  take_changes(): Changes { return { openmls: [], groups: [], epochs: [], outbox: [] }; }
  free(): void {}
}

const parentAgent: WasmAgent = { execute: () => null, take_changes: () => ({ openmls: [], groups: [], epochs: [], outbox: [] }), free: () => {} };

function fixture(options: { group?: boolean } = {}) {
  const server = new FakeServer();
  const world: { agent?: FakeVoiceAgent } = {};
  // The agent survives engine reloads: memory-only state of one call.
  let agent: FakeVoiceAgent | undefined;
  const factory: AgentFactory = { create: () => (agent ??= new FakeVoiceAgent(world)), load: () => agent! };
  const parent = new Engine({ create: () => parentAgent, load: () => parentAgent }, memoryStorage());
  void parent.run(async () => {});
  if (options.group) { server.group = b64(9); server.members = [member(0, "bob")]; server.commit([member(0, "bob")]); }
  const errors: unknown[] = [];
  const host: VoiceHost = {
    server: SERVER, uid: "self-uid", session: 5,
    identity: { nickname: "self", publicKey: DEVICES.self, sign: async () => encodeBase64Url(new Uint8Array(64)) } as unknown as Identity,
    request: (async (op: string, data: Record<string, unknown>) => {
      server.requests.push(op);
      switch (op) {
        case "voice.state": return { voice: { channel: 7, call: CALL, participation: PART, encrypted: true, channel_group: PARENT, group: server.group, epoch: server.group ? server.head : null } };
        case "mls.group.create": {
          if (server.group) throw new RequestError("conflict", server.group);
          server.group = dec<{ create: string }>([...decodeBase64Url(data.group_info as string)]).create; server.members = [member(0, "self")];
          return { group: server.group };
        }
        case "mls.group_info": return { group_info: encodeBase64Url(new Uint8Array(enc({ epoch: server.head, members: server.members, group: server.group }))), epoch: server.head };
        case "mls.log": {
          const since = data.since as number;
          return { head: server.head, oldest: 0, entries: server.log.filter(e => e.epoch >= since), pending: server.pending, next: null };
        }
        case "mls.commit": {
          if (data.epoch !== server.head) throw new RequestError("stale", "stale");
          // Committing by reference clears the server's pending removals.
          server.log.push({ epoch: server.head, commit: data.commit as string, proposals: server.pending });
          server.pending = [];
          server.members = dec<{ members: Member[] }>([...decodeBase64Url(data.commit as string)]).members;
          return { epoch: server.head };
        }
        case "voice.epoch.ack": server.acks.push(data.epoch as number); return {};
        case "voice.epoch.repeat": server.repeats++; return {};
        default: throw new Error(`unexpected request ${op}`);
      }
    }) as VoiceHost["request"],
    tree: async () => ({ epoch: server.head, tree: [] }),
    revocations: () => [], now: () => Date.now(),
    parent, parentGroup: () => PARENT,
    syncParent: async () => true,
    factory, onError: e => errors.push(e),
  };
  const state = { channel: 7, call: CALL, participation: PART, encrypted: true, channel_group: PARENT, group: server.group, epoch: server.group ? server.head : null };
  const call = new VoiceCall(host, state);
  const controls: Control[] = [];
  let run = 0;
  const media = (resumeAfter: string | null = call.resumeAfter()) => {
    run++;
    const engine = new SframeState({ binding: { server_key: SERVER, session: 5, channel: 7, call: CALL, participation: PART, run, group: server.group ?? b64(0) }, resumeAfter: resumeAfter === null ? null : BigInt(resumeAfter), now: () => performance.now(), wallNow: () => Date.now(), onStatus: s => call.engineStatus(s) });
    // The engine's binding names the group the controller settled on.
    const control = (body: Control) => { controls.push(body); return engine.control({ ...body, binding: { ...body.binding } }); };
    call.attach({ run, control });
    return engine;
  };
  return { server, world, call, controls, media, errors, agent: () => world.agent! };
}

/** Advances fake time while letting real WebCrypto work (thread pool) complete. */
const settle = async (ms = 50) => {
  for (let i = 0; i < 10; i++) {
    await vi.advanceTimersByTimeAsync(ms / 10);
    for (let j = 0; j < 20; j++) await new Promise(resolve => setImmediate(resolve));
  }
};
const actions = (controls: Control[]) => controls.map(c => `${c.action}:${c.epoch}`);

describe("VoiceCall", () => {
  beforeEach(() => vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] }));
  afterEach(() => vi.useRealTimers());

  it("creates the singleton, settles against voice.state and activates on its own install", async () => {
    const f = fixture();
    f.call.start();
    await settle();
    expect(await f.call.joined).toBe(f.server.group);
    expect(f.server.requests.slice(0, 2)).toEqual(["mls.group.create", "voice.state"]);
    expect(f.call.roster()).toEqual([{ uid: "self-uid", devices: [DEVICES.self] }]);
    f.media();
    await settle();
    expect(actions(f.controls)).toEqual(["supersede:0", "install:0", "activate:0"]);
    expect(f.server.acks).toEqual([0]);
    expect(f.call.status()).toBe("active");
  });

  it("joins by external commit and activates when every device acknowledged, or at 2 s", async () => {
    const f = fixture({ group: true });
    f.call.start();
    await settle();
    expect(await f.call.joined).toBe(f.server.group);
    expect(f.server.log.length).toBe(2); // bob's creation entry, then this join
    f.media();
    await settle();
    expect(actions(f.controls)).toEqual(["supersede:2", "install:2"]);
    expect(f.call.status()).toBe("securing");
    // Bob's acknowledgement counts only for his device in this validated epoch.
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.carol } });
    await settle();
    expect(f.call.status()).toBe("securing");
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    expect(actions(f.controls)).toEqual(["supersede:2", "install:2", "activate:2"]);
    expect(f.call.status()).toBe("active");

    // Without Bob's acknowledgement the wait ends 2 s after processing.
    const g = fixture({ group: true });
    g.call.start(); await settle(); g.media(); await settle();
    expect(g.call.status()).toBe("securing");
    await settle(ACK_WAIT_MS - 200);
    expect(g.call.status()).toBe("securing");
    await settle(400);
    expect(g.call.status()).toBe("active");
  });

  it("holds before processing a removal, purges the device and activates at once", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media();
    f.call.event({ ev: "voice.epoch.ack", d: { call: f.server.group ? CALL : "", group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    expect(f.call.status()).toBe("active");
    f.controls.length = 0;
    // Bob leaves: the remaining device commits without him (here: the server log has it).
    f.server.commit([member(1, "self")]);
    f.call.event({ ev: "mls.commit", d: { group: f.server.group } });
    await settle();
    expect(actions(f.controls)).toEqual(["hold:3", "supersede:3", "install:3", "activate:3"]);
    expect((f.controls[0] as Extract<Control, { action: "hold" }>).removed).toEqual([DEVICES.bob]);
    expect((f.controls[2] as Extract<Control, { action: "install" }>).removed).toEqual([DEVICES.bob]);
  });

  it("activates a removal epoch without waiting for the remaining devices' acknowledgements", async () => {
    const f = fixture({ group: true });
    f.server.commit([member(0, "bob"), member(2, "carol")]);
    f.call.start(); await settle(); f.media();
    for (const who of ["bob", "carol"] as const) f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 3, device: DEVICES[who] } });
    await settle();
    expect(f.call.status()).toBe("active");
    f.controls.length = 0;
    f.server.commit([member(0, "bob"), member(3, "self")]);
    f.call.event({ ev: "mls.commit", d: { group: f.server.group } });
    await settle();
    // Bob has not acknowledged epoch 4, yet the removal cutoff activates it at once.
    expect(actions(f.controls)).toEqual(["hold:4", "supersede:4", "install:4", "activate:4"]);
  });

  it("buffers acknowledgements that arrive before the epoch is processed", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media(); await settle();
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    f.controls.length = 0;
    // Carol joins; her and Bob's acks for epoch 3 arrive before this device processes it.
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 3, device: DEVICES.bob } });
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 3, device: DEVICES.carol } });
    f.server.commit([member(0, "bob"), member(1, "self"), member(2, "carol")]);
    f.call.event({ ev: "mls.commit", d: { group: f.server.group } });
    await settle();
    expect(actions(f.controls)).toEqual(["supersede:3", "install:3", "activate:3"]);
    // Acks from another call or group are ignored.
    f.call.event({ ev: "voice.epoch.ack", d: { call: b64(9, 16), group: f.server.group, epoch: 3, device: DEVICES.bob } });
    f.call.event({ ev: "voice.epoch.request", d: { call: CALL, group: f.server.group, epoch: 3 } });
    await settle();
    expect(f.server.acks.filter(e => e === 3).length).toBe(2); // installation + the repeat request
  });

  it("after an engine restart installs only from this device's own update", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media();
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    expect(f.call.resumeAfter()).toBe("2");
    f.controls.length = 0;
    const engine = f.media("2");
    await settle(ACK_WAIT_MS + 500);
    // The own update makes epoch 3; nothing at or below 2 is installed in the new run.
    expect(f.server.log.length).toBe(3);
    expect(actions(f.controls)).toEqual(["supersede:3", "install:3", "activate:3"]);
    expect(engine.sending).toBe(true);
  });

  it("holds while the parent gate is blocked and removes a parent-missing leaf after sync", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media();
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    f.controls.length = 0;
    f.agent().parentMissing = [0];
    f.call.event({ ev: "mls.commit", d: { group: PARENT } });
    await settle();
    expect(actions(f.controls).slice(0, 2)).toEqual(["hold:2", "hold:3"]);
    expect(actions(f.controls)).toContain("install:3");
    expect(f.server.members.map(m => m.device_key)).toEqual([DEVICES.self]);
  });

  it("commits a pending server removal and holds first", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media();
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    f.controls.length = 0;
    f.server.pending = ["cmVtb3Zl"];
    f.call.event({ ev: "mls.proposal", d: { group: f.server.group } });
    await settle();
    f.server.pending = [];
    expect(actions(f.controls)[0]).toBe("hold:2");
    expect(f.server.log.length).toBe(3);
  });

  it("closes the gate on a revocation without any event", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media();
    f.call.event({ ev: "voice.epoch.ack", d: { call: CALL, group: f.server.group, epoch: 2, device: DEVICES.bob } });
    await settle();
    expect(f.call.status()).toBe("active");
    f.controls.length = 0;
    f.agent().blocked = true; // e.g. a revoked device or an expired certificate
    await settle(10_500);
    expect(actions(f.controls)[0]).toBe("hold:2");
    expect(f.call.status()).toBe("securing");
  });

  it("closes without further requests or controls", async () => {
    const f = fixture({ group: true });
    f.call.start(); await settle(); f.media(); await settle();
    const requests = f.server.requests.length, controls = f.controls.length;
    f.call.close();
    f.call.event({ ev: "mls.commit", d: { group: f.server.group } });
    await settle(5_000);
    expect(f.server.requests.length).toBe(requests);
    expect(f.controls.length).toBe(controls);
    expect(f.call.roster()).toBeNull();
  });
});
