/** Owns local-only capture, including acquisitions that have not returned yet. */
type Stop = () => void | Promise<void>;
interface Run { retire(): Promise<void> }

export class MicTests {
  private runs = new Set<Run>();
  private generation = 0;

  start(open: (signal: AbortSignal, complete: () => Promise<boolean>) => Promise<Stop>, signal?: AbortSignal): Promise<() => void> {
    const retired = this.stop();
    const generation = this.generation;
    const controller = new AbortController();
    let completed = false;
    let stop: Stop | null = null;
    let cleanup: Promise<void> | null = null;
    const close = () => {
      if (cleanup) return cleanup;
      const fn = stop;
      stop = null;
      // Stop tracks synchronously; wait for native cleanup before handing capture to a call.
      let released: void | Promise<void> = undefined;
      try { released = fn?.(); } catch { /* Retirement must still release ownership. */ }
      cleanup = Promise.resolve(released).catch(() => {}).finally(() => {
        signal?.removeEventListener("abort", abort);
        this.runs.delete(run);
      });
      return cleanup;
    };
    // Completion is run-local: a replacement or a late cancellation cannot finish its successor.
    const complete = async () => {
      if (controller.signal.aborted) return false;
      completed = true;
      await run.retire();
      return completed && generation === this.generation && !signal?.aborted;
    };
    const ready = retired.then(() => controller.signal.aborted ? () => {} : open(controller.signal, complete)).then(fn => {
      stop = fn;
      if (controller.signal.aborted) void close();
    }).catch(error => {
      signal?.removeEventListener("abort", abort);
      this.runs.delete(run);
      if (!controller.signal.aborted) throw error;
    });
    const run: Run = { retire: () => {
      controller.abort();
      if (stop || cleanup) return close();
      return ready.then(close, () => {});
    } };
    const abort = () => { completed = false; void run.retire(); };
    this.runs.add(run);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    return ready.then(() => abort);
  }

  /** Cancels immediately and waits for every outstanding acquisition to release its device. */
  async stop(): Promise<void> {
    ++this.generation;
    await Promise.all([...this.runs].map(run => run.retire()));
  }
}
