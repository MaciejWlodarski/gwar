import "fake-indexeddb/auto";
import { describe, expect, it } from "vitest";
import { MessageStorage, MAX_CHECKPOINTS, MAX_REVOCATIONS } from "./message-storage";
import { keyFromSeed, signText, revokeStatement } from "../connect/crypto";
import { CONNECT_URL } from "../lib/official";

async function seed(name: string, state: unknown) {
  const db = await new Promise<IDBDatabase>((resolve) => { const r = indexedDB.open(name, 1); r.onupgradeneeded = () => r.result.createObjectStore("state"); r.onsuccess = () => resolve(r.result); });
  await new Promise<void>((resolve) => { const tx = db.transaction("state", "readwrite"); tx.objectStore("state").put(state, "auth"); tx.oncomplete = () => resolve(); }); db.close();
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
  it("uses account/device pairing and persists failure of a previously fresh feed", async () => {
    const account = await keyFromSeed(new Uint8Array(32).fill(1)), device = await keyFromSeed(new Uint8Array(32).fill(2));
    const e = { seq: 1, account_key: account.publicKey, device_key: device.publicKey, revoked_at: 2, signature: await signText(account.jwk, revokeStatement(account.publicKey, device.publicKey, 2)) };
    const store = new MessageStorage(crypto.randomUUID()); await store.mergeFeed([e]);
    expect((await store.revocation(account.publicKey, device.publicKey)).revokedAt).toBe(2);
    expect((await store.revocation(device.publicKey, device.publicKey)).revokedAt).toBeUndefined();
    expect((await store.revocation(account.publicKey, device.publicKey)).fresh).toBe(true);
    await store.unavailable(); expect((await store.revocation(account.publicKey, device.publicKey)).fresh).toBe(false);
    expect((await store.revocation(account.publicKey, device.publicKey)).revokedAt).toBe(2);
  });
});
