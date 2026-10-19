import "fake-indexeddb/auto";
import { describe, expect, it, vi } from "vitest";
import { MessageStorage, MAX_CHECKPOINTS, MAX_REVOCATIONS } from "./message-storage";
import { keyFromSeed, signText, revokeStatement } from "../connect/crypto";
import { CONNECT_URL } from "../lib/official";

async function seed(name: string, state: { checkpoints: Record<string, unknown>; revocations: Record<string, unknown>; feeds: Record<string, unknown> }) {
  const db = await new Promise<IDBDatabase>((resolve) => {
    const r = indexedDB.open(name, 2);
    r.onupgradeneeded = () => {
      r.result.createObjectStore("checkpoints").createIndex("at", "at");
      r.result.createObjectStore("revocations"); r.result.createObjectStore("feeds");
    };
    r.onsuccess = () => resolve(r.result);
  });
  await new Promise<void>((resolve) => {
    const tx = db.transaction(["checkpoints", "revocations", "feeds"], "readwrite");
    for (const store of ["checkpoints", "revocations", "feeds"] as const) {
      for (const [key, value] of Object.entries(state[store])) tx.objectStore(store).put(value, key);
    }
    tx.oncomplete = () => resolve();
  }); db.close();
}
const checkpoint = (revision = 0, hash = "hash", at = 1) => ({ revision, hash, conversation: "channel:7", created: 1, at });

describe("atomic bounded message storage", () => {
  it("serializes conflicting revisions across independent clients of the same database", async () => {
    const name = crypto.randomUUID(), a = new MessageStorage(name), b = new MessageStorage(name);
    expect((await a.checkpoint("key", checkpoint(), "-")).continuity).toBe("proven");
    const results = await Promise.all([a.checkpoint("key", checkpoint(1, "a"), "hash"), b.checkpoint("key", checkpoint(1, "b"), "hash")]);
    expect(results.map((r) => r.continuity).sort()).toEqual(["fork", "proven"]);
    expect((await b.checkpoint("key", checkpoint(), "-")).continuity).toBe("rollback");
  });
  it("evicts the oldest checkpoint at the explicit bound", async () => {
    const name = crypto.randomUUID(), checkpoints: Record<string, unknown> = {};
    for (let i = 0; i < MAX_CHECKPOINTS; i++) checkpoints[`key-${i}`] = { ...checkpoint(0, `hash-${i}`, i), proven: true };
    await seed(name, { checkpoints, revocations: {}, feeds: {} });
    const store = new MessageStorage(name);
    await store.checkpoint("new", checkpoint(0, "new", MAX_CHECKPOINTS), "-");
    // Evicted history cannot prove the next predecessor; retained history can reject rollback.
    expect((await store.checkpoint("key-0", checkpoint(1, "next", MAX_CHECKPOINTS + 1), "hash-0")).continuity).toBe("unproven");
    expect((await store.checkpoint(`key-${MAX_CHECKPOINTS - 1}`, checkpoint(0, "different"), "-")).continuity).toBe("fork");
  });
  it("never evicts a revocation at capacity, and commits neither a new cursor nor freshness", async () => {
    const name = crypto.randomUUID(), revocations: Record<string, unknown> = {};
    for (let i = 0; i < MAX_REVOCATIONS; i++) revocations[`retained-${i}`] = { revoked_at: i };
    await seed(name, { checkpoints: {}, revocations, feeds: { [CONNECT_URL]: { cursor: 0, checked: 1, available: false } } });
    const account = await keyFromSeed(new Uint8Array(32).fill(1)), device = await keyFromSeed(new Uint8Array(32).fill(2));
    const e = { seq: 1, account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2, signature: await signText(account.jwk, revokeStatement(account.publicKey, device.publicKey, 2)) };
    const store = new MessageStorage(name);
    await expect(store.mergeFeed([e])).rejects.toThrow(); expect(await store.cursor()).toBe(0);
    expect((await store.revocation(account.publicKey, device.publicKey)).fresh).toBe(false);
  });
  it("counts a feed checked just after a verification began as fresh", async () => {
    const store = new MessageStorage(crypto.randomUUID());
    const account = await keyFromSeed(new Uint8Array(32).fill(1)), device = await keyFromSeed(new Uint8Array(32).fill(2));
    const started = Date.now();
    // The verification took `now` before awaiting the refresh, which then wrote a later `checked`.
    await store.mergeFeed([], undefined, started + 300);
    expect((await store.revocation(account.publicKey, device.publicKey, undefined, started)).fresh).toBe(true);
    expect((await store.revocation(account.publicKey, device.publicKey, undefined, started + 6 * 60_000)).fresh).toBe(false);
  });
  it("uses account/device pairing and persists failure of a previously fresh feed", async () => {
    const account = await keyFromSeed(new Uint8Array(32).fill(1)), device = await keyFromSeed(new Uint8Array(32).fill(2));
    const e = { seq: 1, account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2, signature: await signText(account.jwk, revokeStatement(account.publicKey, device.publicKey, 2)) };
    const store = new MessageStorage(crypto.randomUUID()); await store.mergeFeed([e]);
    expect((await store.revocation(account.publicKey, device.publicKey)).revokedAt).toBe(2);
    expect(store.currentRevocations()).toEqual([{ account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2 }]);
    expect(store.signedRevocations()).toEqual([{ account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2, signature: e.signature }]);
    expect((await store.revocation(device.publicKey, device.publicKey)).revokedAt).toBeUndefined();
    expect((await store.revocation(account.publicKey, device.publicKey)).fresh).toBe(true);
    await store.unavailable(); expect((await store.revocation(account.publicKey, device.publicKey)).fresh).toBe(false);
    expect((await store.revocation(account.publicKey, device.publicKey)).revokedAt).toBe(2);
    expect(store.currentRevocations()).toEqual([{ account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2 }]);
    expect(store.signedRevocations()).toEqual([{ account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2, signature: e.signature }]);
  });
});

describe("per-record storage work", () => {
  it("writes only the affected checkpoint, with no write on an exact duplicate", async () => {
    const store = new MessageStorage(crypto.randomUUID());
    await store.cursor(); // Open the database before counting application transactions.
    const transactions = vi.spyOn(IDBDatabase.prototype, "transaction");
    const puts = vi.spyOn(IDBObjectStore.prototype, "put");
    try {
      await store.checkpoint("server:author:message", checkpoint(), "-");
      await store.checkpoint("server:author:message", checkpoint(), "-");
      expect(puts).toHaveBeenCalledOnce();
      expect(puts.mock.calls[0]?.[1]).toBe("server:author:message");
      expect(transactions).toHaveBeenCalledTimes(2);
      expect(transactions.mock.calls.every(([stores, mode]) => JSON.stringify(stores) === '["checkpoints","order"]' && mode === "readwrite")).toBe(true);
    } finally { puts.mockRestore(); transactions.mockRestore(); }
  });
  it("reads thread authorization in one transaction and aborts an extra feed at the eight-service bound", async () => {
    const store = new MessageStorage(crypto.randomUUID());
    for (let i = 0; i < 8; i++) await store.mergeFeed([], `https://connect-${i}.example`, 1);
    await expect(store.mergeFeed([], "https://extra.example", 2)).rejects.toThrow("Revocation service storage full");
    const transactions = vi.spyOn(IDBDatabase.prototype, "transaction");
    try {
      const states = await store.revocations([{ account: "a", device: "b" }, { account: "c", device: "d" }], "https://connect-0.example", 1);
      expect(states.size).toBe(2); expect([...states.values()].every((s) => s.fresh)).toBe(true);
      expect(transactions).toHaveBeenCalledExactlyOnceWith(["revocations", "feeds"], "readonly", { durability: "strict" });
    } finally { transactions.mockRestore(); }
  });
});
