import { emptyChanges, type AgentFactory, type Change, type Changes, type Job, type WasmAgent } from "./types";
import type { Persistence } from "./storage";
const enc = new TextEncoder(), dec = new TextDecoder("utf-8", { fatal: true });
export const jobKey = (job: Job): string => job.kind === "archive" ? "@runtime/archive" : job.kind === "members" ? `@runtime/members/${job.server}/${job.conversation}` : job.kind === "packages" ? `@runtime/packages/${job.server}`
  : job.kind === "request" ? `@runtime/request/${job.server}/${job.group}` : `@runtime/history/${job.server}/${job.group}/${job.recipient}`;
export const jobChange = (job: Job, remove = false): Change => ({ key: [...enc.encode(jobKey(job))], value: remove ? null : [...enc.encode(JSON.stringify(job))] });
export class AgentOperationError extends Error {
  constructor(cause: unknown) { super(String(cause), { cause }); this.name = "AgentOperationError"; }
}
/** Owned by one lock holder. Never expose the agent outside the serial queue. */
export class Engine {
  private agent: WasmAgent | undefined;
  readonly jobs = new Map<string, Job>();
  private tail: Promise<unknown> = Promise.resolve();
  private stopping = false;
  constructor(private readonly factory: AgentFactory, private readonly storage: Persistence) {}
  async reload(): Promise<void> {
    this.agent?.free(); this.agent = undefined;
    this.jobs.clear();
    const records = await this.storage.load();
    records.outbox = records.outbox.filter(record => {
      const key = dec.decode(new Uint8Array(record.key));
      if (!key.startsWith("@runtime/")) return true;
      const job = JSON.parse(dec.decode(new Uint8Array(record.value!))) as Job;
      if (!["history", "packages", "request", "archive", "members"].includes(job.kind) || jobKey(job) !== key) throw new Error("invalid MLS runtime outbox");
      this.jobs.set(key, job);
      return false;
    });
    if (records.groups.length) this.agent = this.factory.load(records);
    else {
      this.agent = this.factory.create();
      try { await this.storage.write(this.agent.take_changes()); }
      catch (error) { this.agent.free(); this.agent = undefined; throw error; }
    }
  }
  run<T>(operation: () => Promise<T>): Promise<T> {
    if (this.stopping) return Promise.reject(new Error("MLS runtime closed"));
    const result = this.tail.then(async () => {
      if (this.stopping) throw new Error("MLS runtime closed");
      try {
        if (!this.agent) await this.reload();
        return await operation();
      } catch (error) {
        // Also reload after a networking error: its durable outbox remains authoritative.
        this.agent?.free(); this.agent = undefined;
        await this.reload();
        throw error;
      }
    });
    this.tail = result.catch(() => {});
    return result;
  }
  /** Synchronous access for a query that also reads another agent (voice
   * keys against the parent channel). Throws while the agent is reloading. */
  withAgent<T>(read: (agent: WasmAgent) => T): T {
    if (!this.agent) throw new Error("MLS agent not loaded");
    return read(this.agent);
  }
  query<T>(request: unknown): T {
    if (!this.agent) throw new Error("MLS agent not loaded");
    return this.agent.execute(request) as T;
  }
  /** A direct agent method that may change state; its changes are persisted like an operation's.
   * Call it inside `run`. */
  async call<T>(method: (agent: WasmAgent) => T): Promise<T> {
    if (!this.agent) throw new Error("MLS agent not loaded");
    let result: T;
    try { result = method(this.agent); } catch (error) { throw new AgentOperationError(error); }
    await this.persist(this.agent.take_changes());
    return result;
  }
  async op<T>(request: unknown, extra?: (result: T) => Change[]): Promise<T> {
    let result: T;
    try { result = this.query<T>(request); } catch (error) { throw new AgentOperationError(error); }
    // Additional history intents and the accepted MLS transition share the write.
    const changes = this.agent!.take_changes();
    changes.outbox.push(...(extra?.(result) ?? []));
    await this.persist(changes);
    return result;
  }
  async saveJobs(changes: Change[]): Promise<void> {
    const batch = emptyChanges(); batch.outbox = changes; await this.persist(batch);
  }
  private async persist(changes: Changes): Promise<void> {
    await this.storage.write(changes);
    for (const { key, value } of changes.outbox) {
      const name = dec.decode(new Uint8Array(key));
      if (!name.startsWith("@runtime/")) continue;
      if (value === null) this.jobs.delete(name);
      else this.jobs.set(name, JSON.parse(dec.decode(new Uint8Array(value))) as Job);
    }
  }
  async close(): Promise<void> {
    this.stopping = true;
    await this.tail; // Includes the running transaction; queued work observes stopping.
    this.agent?.free(); this.agent = undefined; this.storage.close();
  }
}
/** An automatic startup must never evict a tab that acquired the device meanwhile. */
export class DeviceAlreadyInUse extends Error {}

/** A lock is held until release, including during automatic network reconnects. */
export async function acquireDeviceLock(device: string, locks: LockManager, signal?: AbortSignal, ifAvailable = false): Promise<() => Promise<void>> {
  let release!: () => void;
  let acquired!: () => void;
  let fail!: (error: unknown) => void;
  const ready = new Promise<void>((resolve, reject) => { acquired = resolve; fail = reject; });
  const held = new Promise<void>(resolve => { release = resolve; });
  // Web Locks forbids combining ifAvailable with signal. The non-blocking callback checks cancellation itself.
  const finished = locks.request(`gwar-mls:${device}`, { mode: "exclusive", ...(ifAvailable ? { ifAvailable: true } : { signal }) }, async lock => {
    if (signal?.aborted) { fail(signal.reason); return; }
    if (ifAvailable && !lock) { fail(new DeviceAlreadyInUse()); return; }
    acquired(); await held;
  });
  void finished.catch(fail);
  await ready;
  return async () => { release(); await finished; };
}
