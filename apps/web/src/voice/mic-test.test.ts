import { expect, it, vi } from "vitest";
import { MicTests } from "./mic-test";

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>(yes => { resolve = yes; });
  return { promise, resolve };
}

it("finishes only after asynchronous cleanup, releasing the run before reporting completion", async () => {
  const tests = new MicTests(), released = deferred(), stop = vi.fn(() => released.promise);
  let finish!: () => Promise<boolean>, signal!: AbortSignal;
  await tests.start(async (current, complete) => { signal = current; finish = complete; return stop; });
  const done = finish();
  expect(signal.aborted).toBe(true);
  expect(stop).toHaveBeenCalledOnce();
  let finished = false; void done.then(() => { finished = true; });
  await Promise.resolve(); expect(finished).toBe(false);
  released.resolve();
  expect(await done).toBe(true);
  expect(await finish()).toBe(false);
  await tests.stop();
  expect(stop).toHaveBeenCalledOnce();
});

it("holds replacement capture behind cleanup and rejects obsolete completion", async () => {
  const tests = new MicTests(), released = deferred();
  let finish!: () => Promise<boolean>;
  await tests.start(async (_signal, complete) => { finish = complete; return () => released.promise; });
  const done = finish(), opened = vi.fn(async () => vi.fn());
  const replacement = tests.start(opened);
  await Promise.resolve(); expect(opened).not.toHaveBeenCalled();
  released.resolve();
  expect(await done).toBe(false);
  await replacement;
  expect(opened).toHaveBeenCalledOnce();
  await tests.stop();
});

it("suppresses completion after cancellation during cleanup and still retires a throwing stop", async () => {
  const tests = new MicTests(), released = deferred(), abort = new AbortController();
  let finish!: () => Promise<boolean>;
  await tests.start(async (_signal, complete) => { finish = complete; return () => released.promise; }, abort.signal);
  const done = finish(); abort.abort(); released.resolve();
  expect(await done).toBe(false);
  await tests.start(async (_signal, complete) => { finish = complete; return () => { throw new Error("closed device"); }; });
  expect(await finish()).toBe(true);
  await expect(tests.stop()).resolves.toBeUndefined();
});
