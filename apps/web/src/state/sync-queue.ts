/**
 * Runs a background sync one at a time. A request while one is waiting joins it; a request while one
 * runs queues the next, so a change made during a sync is synced after it.
 */
export function syncQueue(step: () => Promise<void>, what: string) {
  let tail: Promise<void> = Promise.resolve();
  let queued = false;

  const sync = (): void => {
    if (queued) return;
    queued = true;
    tail = tail
      .then(() => {
        queued = false;
        return step();
      })
      .catch((e: unknown) => console.warn(`${what} skipped`, e));
  };

  /** Resolves once nothing is running or queued, counting runs queued meanwhile. */
  const idle = async (): Promise<void> => {
    let seen: Promise<void>;
    do {
      seen = tail;
      await seen;
    } while (seen !== tail);
  };

  /** Syncs until `done()` holds, at most `attempts` times; false if it still does not (offline, failing syncs). */
  const flush = async (done: () => boolean, attempts = 3): Promise<boolean> => {
    for (let attempt = 0; attempt < attempts && !done(); attempt++) {
      sync();
      await idle();
    }
    return done();
  };

  return { sync, idle, flush };
}
