import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { create } from "zustand";
import { CHECK_EVERY_MS, FIRST_CHECK_MS, Updater, type DownloadEvent, type PendingUpdate, type UpdateBackend, type UpdateStatus } from "./updater";

/** A release the fake feed announces; its download is driven by the test. */
class FakeUpdate implements PendingUpdate {
  closed = false;
  installed = false;
  onEvent: ((e: DownloadEvent) => void) | null = null;
  private finish: { resolve: () => void; reject: (e: Error) => void } | null = null;
  installError: Error | null = null;
  constructor(readonly version: string) {}
  download(onEvent: (e: DownloadEvent) => void): Promise<void> {
    this.onEvent = onEvent;
    return new Promise((resolve, reject) => (this.finish = { resolve, reject }));
  }
  complete() { this.finish!.resolve(); }
  fail(message: string) { this.finish!.reject(new Error(message)); }
  async install() {
    if (this.installError) throw this.installError;
    this.installed = true;
  }
  async close() { this.closed = true; }
}

let feed: Array<FakeUpdate | null | Error>;
let backend: UpdateBackend & { relaunch: ReturnType<typeof vi.fn> };
let store: ReturnType<typeof makeStore>;
let updater: Updater;

const makeStore = () => create<{ status: UpdateStatus }>()(() => ({ status: { phase: "none" } }));
const status = () => store.getState().status;
/** Lets pending promise callbacks run. */
const settle = () => new Promise<void>((r) => setTimeout(r, 0));

beforeEach(() => {
  feed = [];
  backend = {
    check: vi.fn(async () => {
      const next = feed.shift() ?? null;
      if (next instanceof Error) throw next;
      return next;
    }),
    relaunch: vi.fn(async () => {}),
  };
  store = makeStore();
  updater = new Updater(backend, store);
  vi.spyOn(console, "warn").mockImplementation(() => {});
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("checking", () => {
  it("shows nothing while the app is up to date or the feed cannot be read", async () => {
    feed.push(null, new Error("offline"));
    await updater.check();
    expect(status()).toEqual({ phase: "none" });
    await updater.check();
    expect(status()).toEqual({ phase: "none" });
  });

  it("downloads a newer version in the background, then waits to be installed", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    expect(status()).toEqual({ phase: "downloading", version: "0.1.3", progress: null });
    update.onEvent!({ event: "Started", data: { contentLength: 1000 } });
    update.onEvent!({ event: "Progress", data: { chunkLength: 425 } });
    expect(status()).toEqual({ phase: "downloading", version: "0.1.3", progress: 0.42 });
    update.onEvent!({ event: "Progress", data: { chunkLength: 575 } });
    update.onEvent!({ event: "Finished" });
    expect(status()).toEqual({ phase: "downloading", version: "0.1.3", progress: 1 });
    update.complete();
    await checked;
    expect(status()).toEqual({ phase: "ready", version: "0.1.3" });
    expect(update.installed).toBe(false);
    expect(backend.relaunch).not.toHaveBeenCalled();
  });

  it("keeps the progress unknown without a size", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    update.onEvent!({ event: "Started", data: {} });
    update.onEvent!({ event: "Progress", data: { chunkLength: 10 } });
    expect(status()).toEqual({ phase: "downloading", version: "0.1.3", progress: null });
    update.complete();
    await checked;
  });

  it("skips a check while a download runs", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    await updater.check();
    expect(backend.check).toHaveBeenCalledTimes(1);
    update.complete();
    await checked;
  });

  it("keeps a downloaded update the feed still announces, and replaces it with a newer one", async () => {
    const first = new FakeUpdate("0.1.3");
    feed.push(first);
    const checked = updater.check();
    await settle();
    first.complete();
    await checked;

    const again = new FakeUpdate("0.1.3");
    feed.push(again);
    await updater.check();
    expect(again.closed).toBe(true);
    expect(first.closed).toBe(false);
    expect(status()).toEqual({ phase: "ready", version: "0.1.3" });

    const newer = new FakeUpdate("0.1.4");
    feed.push(newer);
    const replaced = updater.check();
    await settle();
    expect(first.closed).toBe(true);
    expect(status()).toMatchObject({ phase: "downloading", version: "0.1.4" });
    newer.complete();
    await replaced;
    expect(status()).toEqual({ phase: "ready", version: "0.1.4" });
  });

  it("forgets a downloaded update the feed no longer announces", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    update.complete();
    await checked;
    feed.push(null);
    await updater.check();
    expect(update.closed).toBe(true);
    expect(status()).toEqual({ phase: "none" });
  });

  it("checks shortly after startup and then every few hours", async () => {
    vi.useFakeTimers();
    const stop = updater.start();
    expect(backend.check).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(FIRST_CHECK_MS);
    expect(backend.check).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(CHECK_EVERY_MS);
    expect(backend.check).toHaveBeenCalledTimes(2);
    stop();
    await vi.advanceTimersByTimeAsync(CHECK_EVERY_MS * 2);
    expect(backend.check).toHaveBeenCalledTimes(2);
  });
});

describe("installing", () => {
  async function ready(version = "0.1.3"): Promise<FakeUpdate> {
    const update = new FakeUpdate(version);
    feed.push(update);
    const checked = updater.check();
    await settle();
    update.complete();
    await checked;
    return update;
  }

  it("installs only when asked, then restarts", async () => {
    const update = await ready();
    await updater.install();
    expect(update.installed).toBe(true);
    expect(backend.relaunch).toHaveBeenCalledTimes(1);
  });

  it.each(["success", "install failure", "relaunch failure"])("records voice intent before install and rolls it back on failure: %s", async outcome => {
    const cancel = vi.fn();
    const update = new FakeUpdate("0.1.3");
    const prepare = vi.fn(async (version: string) => { expect(version).toBe("0.1.3"); expect(update.installed).toBe(false); return cancel; });
    updater = new Updater(backend, store, prepare);
    feed.push(update);
    const checked = updater.check(); await settle(); update.complete(); await checked;
    if (outcome === "install failure") update.installError = new Error("denied");
    if (outcome === "relaunch failure") backend.relaunch.mockRejectedValueOnce(new Error("cannot restart"));
    await updater.install();
    expect(prepare).toHaveBeenCalledOnce();
    expect(cancel).toHaveBeenCalledTimes(outcome === "success" ? 0 : 1);
  });

  it("does nothing before the download is ready", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    await updater.install();
    expect(update.installed).toBe(false);
    expect(backend.relaunch).not.toHaveBeenCalled();
    update.complete();
    await checked;
  });

  it("reports a failed install and downloads again on retry", async () => {
    const update = await ready();
    update.installError = new Error("permission denied");
    await updater.install();
    expect(status()).toEqual({ phase: "failed", version: "0.1.3", error: "permission denied" });
    expect(backend.relaunch).not.toHaveBeenCalled();

    const again = new FakeUpdate("0.1.3");
    feed.push(again);
    const retried = updater.retry();
    await settle();
    expect(update.closed).toBe(true);
    expect(status()).toMatchObject({ phase: "downloading", version: "0.1.3" });
    again.complete();
    await retried;
    expect(status()).toEqual({ phase: "ready", version: "0.1.3" });
  });

  it("reports a failed download", async () => {
    const update = new FakeUpdate("0.1.3");
    feed.push(update);
    const checked = updater.check();
    await settle();
    update.fail("signature verification failed");
    await checked;
    expect(status()).toEqual({ phase: "failed", version: "0.1.3", error: "signature verification failed" });
  });
});
