// @vitest-environment happy-dom
import "fake-indexeddb/auto";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CONNECT_URL } from "../lib/official";

const PREFIX = `gwar.connect.handle-pin:${CONNECT_URL}:`;
type Pins = typeof import("./handle-pins");
type State = typeof import("../lib/local-state");

let pins: Pins, state: State;
const others: { close(): void }[] = [];
/** Another tab: its own view of the same database. */
async function otherTab() {
  const tab = state.createLocalState();
  others.push(tab);
  await tab.load();
  return tab;
}
const stored = (handle: string) => JSON.parse(state.localState.get(PREFIX + handle) ?? "null")?.uid ?? null;
const deleted = () => new Promise<void>((resolve, reject) => { const r = indexedDB.deleteDatabase("gwar-local"); r.onsuccess = () => resolve(); r.onerror = () => reject(r.error); });

describe("handle continuity", () => {
  beforeEach(async () => {
    vi.resetModules();
    state = await import("../lib/local-state");
    await state.localState.load();
    pins = await import("./handle-pins");
  });
  afterEach(async () => {
    for (const tab of others.splice(0)) tab.close();
    state.localState.close();
    await deleted();
  });

  it("pins the first account seen behind a handle", async () => {
    expect(pins.handleStatus("alice", "A")).toBe("pending");
    await pins.rememberHandle("alice", "A");
    expect(pins.handleStatus("alice", "A")).toBe("verified");
    expect(pins.handleStatus("alice", "B")).toBe("changed");
  });

  it("never lets a later sighting replace the pin", async () => {
    await pins.rememberHandle("alice", "A");
    await pins.rememberHandle("alice", "B");
    expect(stored("alice")).toBe("A");
    expect(pins.handleStatus("alice", "B")).toBe("changed");
  });

  it("moves the pin only when the user accepts the new account", async () => {
    await pins.rememberHandle("alice", "A");
    expect(await pins.acceptHandle("alice", "B")).toBe(true);
    expect(pins.handleStatus("alice", "B")).toBe("verified");
    expect(pins.handleStatus("alice", "A")).toBe("changed");
  });

  it("treats a corrupt entry as unpinned, and pins over it", async () => {
    await state.localState.update(PREFIX + "alice", () => "{nope");
    await state.localState.update(PREFIX + "bob", () => JSON.stringify({ uid: 5 }));
    expect(pins.handleStatus("alice", "B")).toBe("pending");
    expect(pins.handleStatus("bob", "B")).toBe("pending");
    await pins.rememberHandle("alice", "B");
    expect(pins.handleStatus("alice", "B")).toBe("verified");
  });

  it("a first sighting yields to a pin another tab stored meanwhile", async () => {
    expect(pins.handleStatus("alice", "B")).toBe("pending");
    await (await otherTab()).update(PREFIX + "alice", () => JSON.stringify({ uid: "A" }));
    // This tab may not have heard of it yet; the transaction decides on what is stored.
    await pins.rememberHandle("alice", "B");
    expect(stored("alice")).toBe("A");
    expect(pins.handleStatus("alice", "B")).toBe("changed");
  });

  it("follows another tab's acceptance, and so does every page loaded after it", async () => {
    await pins.rememberHandle("alice", "A");
    const changes = vi.fn();
    pins.subscribeHandlePins(changes);
    const tab = await otherTab();
    await tab.update(PREFIX + "alice", () => JSON.stringify({ uid: "B" }));
    await vi.waitFor(() => expect(pins.handleStatus("alice", "B")).toBe("verified"));
    expect(pins.handleStatus("alice", "A")).toBe("changed");
    expect(changes).toHaveBeenCalled();
    // A reload reads what was committed.
    const reloaded = await otherTab();
    expect(JSON.parse(reloaded.get(PREFIX + "alice")!).uid).toBe("B");
  });

  it("two tabs accepting different accounts end on the one committed last, everywhere", async () => {
    await pins.rememberHandle("alice", "A");
    const tab = await otherTab();
    // Transactions on one store run in the order they were made: this tab's, then the other's.
    await Promise.all([pins.acceptHandle("alice", "B"), tab.update(PREFIX + "alice", () => JSON.stringify({ uid: "C" }))]);
    const last = "C";
    await vi.waitFor(() => expect(stored("alice")).toBe(last));
    await vi.waitFor(() => expect(JSON.parse(tab.get(PREFIX + "alice")!).uid).toBe(last));
    expect(JSON.parse((await otherTab()).get(PREFIX + "alice")!).uid).toBe(last);
  });

  it("while the security state cannot be used, nothing is pinned or shown as verified", async () => {
    await pins.rememberHandle("alice", "A");
    // A newer version's tab upgrades the database: this page lets go of it.
    await new Promise<void>((resolve, reject) => {
      const r = indexedDB.open("gwar-local", 2);
      r.onsuccess = () => { r.result.close(); resolve(); };
      r.onerror = () => reject(r.error);
    });
    expect(state.localState.error()).toMatchObject({ name: "NewerDataError" });
    expect(pins.handleStatus("alice", "A")).toBe("pending");
    await pins.rememberHandle("bob", "B");
    expect(pins.handleStatus("bob", "B")).toBe("pending");
    expect(await pins.acceptHandle("alice", "B")).toBe(false);
    expect(pins.handleStatus("alice", "B")).toBe("pending");
  });
});
