import { IDBFactory, IDBObjectStore } from "fake-indexeddb";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { certifyDevice, generateKeyPair } from "../connect/crypto";
import { indexedDbConnectStore, indexedDbIdentityStore, loadActiveIdentity, type ConnectRecord } from "./identity";

beforeEach(() => vi.stubGlobal("indexedDB", new IDBFactory()));
afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

async function enrollment(): Promise<ConnectRecord> {
  const root = await loadActiveIdentity();
  const backup = root.exportBackup();
  const device = await generateKeyPair();
  const certificate = await certifyDevice({ jwk: backup.jwk, publicKey: root.publicKey }, device.publicKey);
  return { version: 1, handle: "alice", accountKey: root.publicKey, deviceName: "browser", deviceJwk: device.jwk,
    certificate: { account_key: root.publicKey, ...certificate }, token: "device-session", vaultKey: "vault-key" };
}

describe("durable root migration", () => {
  it("saves the device and removes the root atomically; sign-out creates a different identity", async () => {
    const record = await enrollment();
    await indexedDbConnectStore.save(record);
    expect(await indexedDbConnectStore.load()).toEqual(record);
    expect(await indexedDbIdentityStore.load()).toBeUndefined();
    expect((await loadActiveIdentity()).publicKey).toBe(record.certificate.device_key);
    await indexedDbConnectStore.clear();
    const signedOut = await loadActiveIdentity();
    expect(signedOut.publicKey).not.toBe(record.accountKey);
    expect(signedOut.device).toBeUndefined();
  });

  it("an aborted enrollment preserves the root and stores no device", async () => {
    const record = await enrollment();
    const put = IDBObjectStore.prototype.put;
    vi.spyOn(IDBObjectStore.prototype, "put").mockImplementation(function (this: IDBObjectStore, value, key) {
      const request = put.call(this, value, key);
      this.transaction.abort();
      return request;
    });
    await expect(indexedDbConnectStore.save(record)).rejects.toThrow();
    expect(await indexedDbConnectStore.load()).toBeUndefined();
    expect((await indexedDbIdentityStore.load())?.jwk.x).toBe(record.accountKey);
  });

  it("keeps an unrelated local identity", async () => {
    const record = await enrollment();
    const other = await generateKeyPair();
    await indexedDbIdentityStore.save({ format: "gwar-identity", version: 1, jwk: other.jwk, nickname: "Other" });
    await indexedDbConnectStore.save(record);
    expect((await indexedDbIdentityStore.load())?.jwk.x).toBe(other.publicKey);
  });
});
