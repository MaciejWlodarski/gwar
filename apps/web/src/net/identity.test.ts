import { describe, expect, it } from "vitest";
import { generateKeyPair, certifyDevice } from "../connect/crypto";
import { decodeBase64Url } from "./base64url";
import {
  accountKeyOf,
  challengeMessage,
  identityFromConnect,
  loadActiveIdentity,
  loadConnectRecord,
  type ConnectRecord,
  type ConnectStore,
  type IdentityBackup,
  type IdentityStore,
} from "./identity";

function memoryStores() {
  let local: IdentityBackup | undefined;
  let connect: ConnectRecord | undefined;
  const localStore: IdentityStore = { load: async () => local, save: async (b) => void (local = b) };
  const connectStore: ConnectStore = {
    load: async () => connect,
    save: async (r) => void (connect = r),
    clear: async () => void (connect = undefined),
  };
  return { localStore, connectStore };
}

async function record(): Promise<ConnectRecord> {
  const account = await generateKeyPair();
  const device = await generateKeyPair();
  const cert = await certifyDevice(account, device.publicKey);
  return {
    version: 1,
    handle: "alice",
    accountKey: account.publicKey,
    deviceName: "Chrome on macOS",
    deviceJwk: { kty: "OKP", crv: "Ed25519", x: device.jwk.x, d: device.jwk.d },
    certificate: { account_key: account.publicKey, ...cert },
    token: "tok",
  };
}

describe("identity switching", () => {
  it("uses the local identity until a Connect record exists, then the Connect one, then local again", async () => {
    const { localStore, connectStore } = memoryStores();
    const local = await loadActiveIdentity(localStore, connectStore);
    expect(local.device).toBeUndefined();
    expect(accountKeyOf(local)).toBe(local.publicKey);

    const rec = await record();
    await connectStore.save(rec);
    const connected = await loadActiveIdentity(localStore, connectStore);
    expect(connected.publicKey).toBe(rec.certificate.device_key);
    expect(connected.device).toEqual(rec.certificate);
    expect(accountKeyOf(connected)).toBe(rec.accountKey);
    expect(connected.publicKey).not.toBe(local.publicKey);

    // Signing out clears the record; the same local identity comes back.
    await connectStore.clear();
    const back = await loadActiveIdentity(localStore, connectStore);
    expect(back.publicKey).toBe(local.publicKey);
    expect(back.device).toBeUndefined();
  });

  it("ignores a broken Connect record", async () => {
    const { localStore, connectStore } = memoryStores();
    const rec = await record();
    await connectStore.save({ ...rec, certificate: { ...rec.certificate, device_key: "someone else" } });
    expect(await loadConnectRecord(connectStore)).toBeUndefined();
    expect((await loadActiveIdentity(localStore, connectStore)).device).toBeUndefined();
  });

  it("a Connect identity signs the hello challenge with the device key and has no portable backup", async () => {
    const rec = await record();
    const id = await identityFromConnect(rec);
    const msg = challengeMessage("nonce", id.publicKey);
    const pub = await crypto.subtle.importKey("raw", decodeBase64Url(rec.certificate.device_key) as BufferSource, { name: "Ed25519" }, false, ["verify"]);
    const ok = await crypto.subtle.verify({ name: "Ed25519" }, pub, decodeBase64Url(await id.sign(msg)) as BufferSource, msg as BufferSource);
    expect(ok).toBe(true);
    expect(() => id.exportBackup()).toThrow();
  });
});
