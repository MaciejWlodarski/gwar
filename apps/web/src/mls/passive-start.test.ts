import { afterEach, expect, it, vi } from "vitest";
import type { Identity } from "../net/identity";
import type { Link } from "../net/connection";
import { DeviceAlreadyInUse } from "./engine";
import { MlsRuntime } from "./runtime";

const posted = vi.fn();
afterEach(() => { vi.unstubAllGlobals(); posted.mockReset(); });

it.each([false, true])("passive=%s claims atomically without evicting an occupied device", async passive => {
  vi.stubGlobal("BroadcastChannel", class {
    onmessage = null;
    postMessage = posted;
    close() {}
  });
  const request = vi.fn(async (_name: string, options: LockOptions, callback: (lock: Lock | null) => Promise<void>) => {
    if (options.ifAvailable) await callback(null);
    else await callback({ name: "gwar-mls:device", mode: "exclusive" } as Lock);
  });
  const connect = vi.fn();
  const storage = vi.fn(async () => { throw new Error("stop before opening any connection"); });
  const options = {
    identity: { publicKey: "device" } as Identity, seed: new Uint8Array(32),
    link: { status: { state: "idle" }, connect } as unknown as Link,
    locks: { request } as unknown as LockManager, storage, revocations: () => [], onTakeover: vi.fn(), takeover: !passive,
  };
  if (passive) {
    await expect(MlsRuntime.connect(options)).rejects.toBeInstanceOf(DeviceAlreadyInUse);
    expect(request).toHaveBeenCalledWith("gwar-mls:device", { mode: "exclusive", ifAvailable: true }, expect.any(Function));
    expect(posted).not.toHaveBeenCalled(); expect(storage).not.toHaveBeenCalled();
  } else {
    await expect(MlsRuntime.connect(options)).rejects.toThrow("stop before opening any connection");
    expect(posted).toHaveBeenCalledOnce();
  }
  expect(connect).not.toHaveBeenCalled();
});

it("passive startup can acquire a free lock without announcing a takeover or combining signal and ifAvailable", async () => {
  const { acquireDeviceLock } = await import("./engine");
  const request = vi.fn(async (_name: string, _options: LockOptions, callback: (lock: Lock) => Promise<void>) => callback({} as Lock));
  const release = await acquireDeviceLock("device", { request } as unknown as LockManager, new AbortController().signal, true);
  expect(request.mock.calls[0]![1]).toEqual({ mode: "exclusive", ifAvailable: true });
  await release();
});
