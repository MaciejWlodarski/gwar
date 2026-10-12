import { describe, expect, it, vi } from "vitest";
import { syncQueue } from "./sync-queue";

const deferred = () => {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => (resolve = r));
  return { promise, resolve };
};

describe("syncQueue", () => {
  it("runs one at a time, joins waiting requests and waits for runs queued meanwhile", async () => {
    const runs: Array<ReturnType<typeof deferred>> = [];
    const queue = syncQueue(() => {
      const run = deferred();
      runs.push(run);
      return run.promise;
    }, "test");
    queue.sync();
    queue.sync();
    await vi.waitFor(() => expect(runs).toHaveLength(1));
    // A change while the first run is busy queues exactly one more.
    queue.sync();
    queue.sync();
    let idle = false;
    const waiting = queue.idle().then(() => (idle = true));
    runs[0]!.resolve();
    await vi.waitFor(() => expect(runs).toHaveLength(2));
    expect(idle).toBe(false);
    // The second run asks for a third (as a sync that saw an edit does): idle waits for it too.
    queue.sync();
    runs[1]!.resolve();
    await vi.waitFor(() => expect(runs).toHaveLength(3));
    expect(idle).toBe(false);
    runs[2]!.resolve();
    await waiting;
    expect(runs).toHaveLength(3);
  });

  it("flush retries failed runs and reports whether it got there", async () => {
    vi.spyOn(console, "warn").mockImplementation(() => {});
    let failures = 2, synced = false;
    const queue = syncQueue(async () => {
      if (failures-- > 0) throw new Error("offline");
      synced = true;
    }, "test");
    expect(await queue.flush(() => synced)).toBe(true);
    expect(failures).toBe(-1);

    const never = syncQueue(() => Promise.reject(new Error("offline")), "test");
    expect(await never.flush(() => false, 2)).toBe(false);
  });
});
