import { describe, expect, it, vi } from "vitest";
import { IDBFactory, IDBObjectStore } from "fake-indexeddb";
import { Engine, acquireDeviceLock, jobChange } from "./engine";
import { openStorage } from "./storage";
import { emptyChanges, type AgentFactory, type Changes, type WasmAgent } from "./types";
const enc = new TextEncoder();
function fakeFactory() {
  const agents: FakeAgent[] = [];
  class FakeAgent implements WasmAgent {
    value: number; freed = false;
    constructor(value: number) { this.value = value; agents.push(this); }
    execute(request: unknown) {
      if (this.freed) throw new Error("freed");
      if (request === "fail") { this.value++; throw new Error("operation failed"); }
      if (request === "increment") this.value++;
      return this.value;
    }
    take_changes(): Changes {
      const changes = emptyChanges();
      for (const store of ["groups", "openmls", "epochs", "outbox"] as const) changes[store] = [{ key: [...enc.encode("value")], value: [this.value] }];
      return changes;
    }
    free() { this.freed = true; }
  }
  const factory: AgentFactory = { create: () => new FakeAgent(0), load: records => new FakeAgent(records.groups[0]!.value![0]!) };
  return { factory, agents };
}
function deferred() {
  let resolve!: () => void; const promise = new Promise<void>(r => { resolve = r; }); return { promise, resolve };
}
/** Models exclusive acquisition and cancellation of waiters, without timers. */
export function fakeLocks(): LockManager {
  let tail = Promise.resolve();
  return { request: (_name: string, options: LockOptions, callback: () => Promise<void>) => {
    const run = tail.then(async () => { if (options.signal?.aborted) throw options.signal.reason; await callback(); });
    tail = run.catch(() => {}); return run;
  } } as LockManager;
}
describe("MLS durable queue", () => {
  it("crash before write loses the operation across all four stores", async () => {
    const storage = await openStorage("crash", new IDBFactory()); const f = fakeFactory();
    const engine = new Engine(f.factory, storage);
    await engine.run(async () => {});
    f.agents[0]!.execute("increment"); // Process dies before take_changes/write.
    await engine.reload();
    expect(engine.query("read")).toBe(0);
    const snapshot = await storage.load();
    expect(Object.values(snapshot).every(records => records[0]!.value![0] === 0)).toBe(true);
    await engine.close();
  });
  it("does not persist partial failed operations and reloads a failed write", async () => {
    const storage = await openStorage("failure", new IDBFactory()); const f = fakeFactory();
    let fail = false;
    const engine = new Engine(f.factory, { ...storage, write: changes => fail ? (fail = false, Promise.reject(new Error("disk full"))) : storage.write(changes) });
    await engine.run(async () => {});
    await expect(engine.run(() => engine.op("fail"))).rejects.toThrow("operation failed");
    expect(engine.query("read")).toBe(0);
    fail = true;
    await expect(engine.run(() => engine.op("increment"))).rejects.toThrow("disk full");
    expect(engine.query("read")).toBe(0);
    expect(f.agents.slice(0, -1).every(a => a.freed)).toBe(true);
    await engine.run(() => engine.op("increment")); expect(engine.query("read")).toBe(1);
    await engine.close();
  });
  it("aborts all stores when a later IndexedDB put fails", async () => {
    const storage = await openStorage("atomic", new IDBFactory()); const f = fakeFactory();
    const engine = new Engine(f.factory, storage); await engine.run(async () => {});
    const put = IDBObjectStore.prototype.put;
    const spy = vi.spyOn(IDBObjectStore.prototype, "put").mockImplementation(function(this: IDBObjectStore, ...args) {
      if (this.name === "epochs") throw new DOMException("quota", "QuotaExceededError");
      return put.apply(this, args);
    });
    try { await expect(engine.run(() => engine.op("increment"))).rejects.toThrow("quota"); }
    finally { spy.mockRestore(); }
    expect(Object.values(await storage.load()).every(records => records[0]!.value![0] === 0)).toBe(true);
    expect(engine.query("read")).toBe(0); await engine.close();
  });
  it("holds serial operations and sends behind transaction completion", async () => {
    const storage = await openStorage("serial", new IDBFactory()); const f = fakeFactory();
    const gate = deferred(), started = deferred(); const actions: string[] = []; let block = false;
    const engine = new Engine(f.factory, { ...storage, write: async changes => { if (block) { started.resolve(); await gate.promise; } await storage.write(changes); } });
    await engine.run(async () => {}); block = true;
    const first = engine.run(async () => { await engine.op("increment"); actions.push("send"); });
    await started.promise;
    const second = engine.run(async () => { actions.push("second"); await engine.op("increment"); });
    expect(actions).toEqual([]); expect(f.agents[0]!.value).toBe(1);
    gate.resolve(); await Promise.all([first, second]); expect(actions).toEqual(["send", "second"]);
    await engine.close();
  });
  it("commits runtime history jobs atomically with MLS and filters them on reload", async () => {
    const storage = await openStorage("jobs", new IDBFactory()); const f = fakeFactory(); const engine = new Engine(f.factory, storage);
    await engine.run(() => engine.op("increment", () => [jobChange({ kind: "history", server: "s", group: "g", recipient: "r", publication: null, chunks: null })]));
    await engine.reload(); expect(engine.query("read")).toBe(1); expect(engine.jobs.size).toBe(1);
    await engine.close();
  });
  it("waits for the current write, rejects queued work and then releases the lock", async () => {
    const locks = fakeLocks(); const release = await acquireDeviceLock("device", locks);
    const storage = await openStorage("closing", new IDBFactory()); const f = fakeFactory(); const gate = deferred(), started = deferred(); let block = false;
    const engine = new Engine(f.factory, { ...storage, write: async changes => { if (block) { started.resolve(); await gate.promise; } await storage.write(changes); } });
    await engine.run(async () => {}); block = true;
    const running = engine.run(() => engine.op("increment")); await started.promise;
    const queued = engine.run(() => engine.op("increment")); const rejected = expect(queued).rejects.toThrow("closed");
    let acquired = false;
    const next = acquireDeviceLock("device", locks).then(r => { acquired = true; return r; });
    const closing = engine.close().then(release);
    await Promise.resolve(); expect(acquired).toBe(false); expect(f.agents[0]!.freed).toBe(false);
    gate.resolve(); await running; await rejected; await closing;
    await (await next)(); expect(acquired).toBe(true); expect(f.agents[0]!.freed).toBe(true);
  });
  it("aborts a waiter without releasing another owner's lock", async () => {
    const locks = fakeLocks(); const release = await acquireDeviceLock("device", locks); const abort = new AbortController();
    const waiting = acquireDeviceLock("device", locks, abort.signal); abort.abort(); await release();
    await expect(waiting).rejects.toThrow();
  });
});
