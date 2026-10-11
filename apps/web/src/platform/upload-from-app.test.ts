import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// The desktop upload helper awaits native readiness and an import before handing bytes to native code; a session
// that ends during either wait must not reach `upload_put` (Astra r4 blocker 1, r5 follow-up).
const native = vi.hoisted(() => ({
  invoke: vi.fn(),
  setServerHeld: null as Promise<void> | null,
}));
vi.mock(".", () => ({
  isDesktop: () => true,
  tauri: async () => ({ invoke: (command: string, args?: unknown) => native.invoke(command, args) }),
}));
const core = () => ({ invoke: (...args: unknown[]) => native.invoke(...args) });

const held = () => {
  let release!: () => void;
  return { promise: new Promise<void>((r) => { release = r; }), release };
};
const path = "/api/files/file?token=x";

describe("desktop upload helper", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.doMock("@tauri-apps/api/core", core);
    native.setServerHeld = null;
    native.invoke = vi.fn(async (command: string) => {
      if (command === "files_set_server") await native.setServerHeld;
      return { id: "file" };
    });
  });
  afterEach(() => { vi.doUnmock("@tauri-apps/api/core"); });

  it("refuses when the session ends while native readiness is pending", async () => {
    const { setConnectedServer, uploadFromApp } = await import("./download");
    const wait = held();
    native.setServerHeld = wait.promise;
    void setConnectedServer("https://b.example", 100);
    let current = true;
    const pending = uploadFromApp(path, new Uint8Array([1]), 100, "https://a.example", () => { if (!current) throw new Error("stale session"); });
    const result = expect(pending).rejects.toThrow("stale session");
    await new Promise((r) => setTimeout(r, 0));
    current = false;
    wait.release();
    await result;
    expect(native.invoke.mock.calls.map(([command]) => command)).toEqual(["files_set_server"]);
  });

  it("refuses when the session ends while the native module is loading", async () => {
    const wait = held();
    vi.doMock("@tauri-apps/api/core", async () => { await wait.promise; return core(); });
    const { uploadFromApp } = await import("./download");
    let current = true;
    const pending = uploadFromApp(path, new Uint8Array([1]), 100, "https://a.example", () => { if (!current) throw new Error("stale session"); });
    const result = expect(pending).rejects.toThrow("stale session");
    await new Promise((r) => setTimeout(r, 0));
    current = false;
    wait.release();
    await result;
    expect(native.invoke).not.toHaveBeenCalled();
  });

  it("checks the session last, then hands native code the same bytes and the reserved origin", async () => {
    const { uploadFromApp } = await import("./download");
    const bytes = new Uint8Array([4, 5, 6]), stillHere = vi.fn();
    await uploadFromApp(path, bytes, 100, "https://a.example", stillHere);
    expect(native.invoke).toHaveBeenCalledExactlyOnceWith("upload_put", bytes, { headers: { "x-upload-path": path, "x-upload-origin": "https://a.example" } });
    expect(native.invoke.mock.calls[0]![1]).toBe(bytes);
    expect(native.invoke.mock.invocationCallOrder[0]).toBeGreaterThan(stillHere.mock.invocationCallOrder[0]!);
  });
});
