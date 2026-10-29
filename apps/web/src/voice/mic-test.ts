/** Owns local-only capture, including acquisitions that have not returned yet. */
type Stop = () => void | Promise<void>;
interface Run { retire(): Promise<void> }

export class MicTests {
  private runs = new Set<Run>();

  start(open: (signal: AbortSignal) => Promise<Stop>, signal?: AbortSignal): Promise<() => void> {
    void this.stop();
    const controller = new AbortController();
    let stop: Stop | null = null;
    let cleanup: Promise<void> | null = null;
    const close = () => {
      if (cleanup) return cleanup;
      const fn = stop;
      stop = null;
      // Stop tracks synchronously; wait for native cleanup before handing capture to a call.
      cleanup = Promise.resolve(fn?.()).catch(() => {}).finally(() => {
        signal?.removeEventListener("abort", abort);
        this.runs.delete(run);
      });
      return cleanup;
    };
    const ready = Promise.resolve().then(() => controller.signal.aborted ? () => {} : open(controller.signal)).then(fn => {
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
    const abort = () => { void run.retire(); };
    this.runs.add(run);
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) abort();
    return ready.then(() => abort);
  }

  /** Cancels immediately and waits for every outstanding acquisition to release its device. */
  async stop(): Promise<void> {
    await Promise.all([...this.runs].map(run => run.retire()));
  }
}
