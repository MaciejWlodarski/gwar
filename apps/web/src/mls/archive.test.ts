import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import init from "../../../../crates/gwar-mls-wasm/pkg/gwar_mls_wasm.js";
import { changeRecoveryCode, createAccount, newRecoveryCode, recover, signIn } from "../connect/account";
import { setup, type FakeService } from "../connect/testing";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import { identityFromConnect, identitySeed, uidForPublicKey, type ConnectRecord } from "../net/identity";
import { KeyArchive, transferGeneration } from "./archive";
import { credential, descriptor } from "./descriptor";
import { Engine } from "./engine";
import type { Persistence } from "./storage";
import type { Change, Changes } from "./types";
import { loadAgent } from "./wasm";

await init({ module_or_path: await readFile(new URL("../../../../crates/gwar-mls-wasm/pkg/gwar_mls_wasm_bg.wasm", import.meta.url)) });

const dec = new TextDecoder();
const server = encodeBase64Url(new Uint8Array(32).fill(7));

function memory(): Persistence & { get(store: keyof Changes, key: string): unknown } {
  const stores: Record<keyof Changes, Map<string, Change>> = { openmls: new Map(), groups: new Map(), epochs: new Map(), outbox: new Map() };
  return {
    load: async () => ({
      openmls: [...stores.openmls.values()], groups: [...stores.groups.values()], epochs: [...stores.epochs.values()], outbox: [...stores.outbox.values()],
    }),
    write: async (changes) => {
      for (const store of Object.keys(stores) as (keyof Changes)[]) {
        for (const change of changes[store]) {
          const key = dec.decode(new Uint8Array(change.key));
          if (change.value === null) stores[store].delete(key);
          else stores[store].set(key, change);
        }
      }
    },
    close: () => {},
    get: (store, key) => {
      const value = stores[store].get(key)?.value;
      return value ? JSON.parse(dec.decode(new Uint8Array(value))) : undefined;
    },
  };
}

async function device(service: FakeService, deps: ReturnType<typeof setup>["deps"], record: ConnectRecord) {
  const identity = await identityFromConnect(record);
  const storage = memory();
  const engine = new Engine(await loadAgent(identitySeed(identity), credential(identity), Date.now), storage);
  const d = {
    identity, engine, storage, record, uploads: true,
    archive: undefined as unknown as KeyArchive,
    /** What the archive stores in the agent (`@archive`). */
    state: () => storage.get("groups", "@archive") as { keys: Record<string, unknown>; held: Record<string, unknown> } | undefined,
  };
  d.archive = new KeyArchive(engine, {
    api: deps().api, deviceKey: identity.publicKey, now: Date.now,
    session: async () => ({ token: d.record.token, accountKey: d.record.accountKey, recovery: d.record.archiveRecovery, recoverySecret: d.record.archiveRecoverySecret }),
    used: async (what) => {
      if (what.recovery && d.record.archiveRecovery?.statement === what.recovery.statement) d.record = { ...d.record, archiveRecovery: undefined };
      if (what.recoverySecret) d.record = { ...d.record, archiveRecoverySecret: undefined };
    },
    uploads: () => d.uploads,
    revocations: () => [],
  });
  void service;
  return d;
}

describe("key archive", () => {
  it("shares a device's epoch keys with the account's other devices and the recovery code", async () => {
    const { service, deps } = setup();
    const created = await createAccount({ handle: "alice", password: "correct horse battery" }, deps("Laptop"));
    expect(created.record.archiveRecovery?.statement.split("\n")[2]).toBe("1");
    const laptop = await device(service, deps, created.record);
    // An epoch key to back up: the laptop's channel group.
    const uid = await uidForPublicKey(created.record.accountKey);
    const d = await descriptor(laptop.identity, uid, server, "channel:1", Date.now());
    const group = [...decodeBase64Url(d.group)];
    await laptop.engine.run(async () => {
      await laptop.engine.op({ create: { descriptor: d.bytes } });
      await laptop.engine.op({ settle: { server, group, winner: group } });
    });

    // The first pass publishes the device and recovery keys, makes generation 1 and uploads.
    await laptop.archive.pass();
    const archive = service.archive.get("alice")!;
    expect(laptop.record.archiveRecovery).toBeUndefined();
    expect(archive.recovery).toHaveLength(1);
    expect([...archive.manifests.keys()]).toEqual([1]);
    expect(archive.records).toHaveLength(1);
    await laptop.archive.pass();
    expect(archive.records).toHaveLength(1);

    // A second device reads nothing until the laptop sends it a transfer.
    const phone = await device(service, deps, await signIn({ handle: "alice", password: "correct horse battery" }, deps("Phone")));
    phone.uploads = false;
    await phone.archive.pass();
    expect(Object.keys(phone.state()!.keys)).toEqual([]);
    await laptop.archive.pass();
    expect(archive.transfers).toHaveLength(1);
    await phone.archive.pass();
    expect(Object.keys(phone.state()!.keys)).toEqual(["1"]);
    // The key of a group the phone does not hold waits for it.
    expect(Object.keys(phone.state()!.held)).toHaveLength(1);
    // Uploads off: the phone made no generation and uploaded nothing.
    expect([...archive.manifests.keys()]).toEqual([1]);

    // Every device lost: the recovery code opens generation 1 on a new one.
    const recovered = await recover({ handle: "alice", code: created.recoveryCode, newPassword: "another horse battery" }, deps("New laptop"));
    expect(recovered.archiveRecoverySecret).toBeDefined();
    const fresh = await device(service, deps, recovered);
    await fresh.archive.pass();
    expect(fresh.record.archiveRecoverySecret).toBeUndefined();
    expect(Object.keys(fresh.state()!.keys)).toContain("1");
    expect(Object.keys(fresh.state()!.held)).toHaveLength(1);
  });

  it("publishes a changed recovery code's key, and transfers older generations to it", async () => {
    const { service, deps, stored } = setup();
    const created = await createAccount({ handle: "bob", password: "correct horse battery" }, deps());
    const laptop = await device(service, deps, created.record);
    await laptop.archive.pass();
    const code = newRecoveryCode();
    const changed = await changeRecoveryCode(created.record, "correct horse battery", code, deps());
    expect(changed.archiveRecovery).toBeUndefined();
    expect(stored()?.archiveRecovery).toBeUndefined();
    const archive = service.archive.get("bob")!;
    expect(archive.recovery.map(k => k.statement.split("\n")[2])).toEqual(["1", "2"]);
    laptop.record = { ...laptop.record, token: changed.token };
    await laptop.archive.pass();
    // Generation 1 went to version 2 by a transfer; generation 2 is sealed to it.
    expect([...archive.manifests.keys()]).toEqual([1, 2]);
    expect(archive.transfers).toHaveLength(1);
    const fresh = await device(service, deps, await recover({ handle: "bob", code, newPassword: "another horse battery" }, deps("New")));
    await fresh.archive.pass();
    // It opens both (and then makes generation 3, which names it).
    expect(Object.keys(fresh.state()!.keys)).toEqual(["1", "2", "3"]);
  });

  it("drops a pending recovery key a newer version replaced", async () => {
    const { service, deps } = setup();
    const created = await createAccount({ handle: "carol", password: "correct horse battery" }, deps());
    const stale = created.record.archiveRecovery!;
    // The code changed before the sign-up key was ever published: version 1 is the new code's.
    const changed = await changeRecoveryCode(created.record, "correct horse battery", newRecoveryCode(), deps());
    const laptop = await device(service, deps, { ...changed, archiveRecovery: stale });
    await laptop.archive.pass();
    expect(laptop.record.archiveRecovery).toBeUndefined();
    const keys = service.archive.get("carol")!.recovery;
    expect(keys).toHaveLength(1);
    expect(keys[0]!.statement).not.toBe(stale.statement);
  });

  it("refuses an archive page whose cursor does not advance", async () => {
    const { service, deps } = setup();
    const created = await createAccount({ handle: "dave", password: "correct horse battery" }, deps());
    const laptop = await device(service, deps, created.record);
    await laptop.archive.pass();
    const api = deps().api;
    api.archiveTransfers = async () => ({ transfers: [], next: 0 });
    const stuck = new KeyArchive(laptop.engine, {
      api, deviceKey: laptop.identity.publicKey, now: Date.now,
      session: async () => ({ token: laptop.record.token, accountKey: laptop.record.accountKey }),
      used: async () => {}, uploads: () => true, revocations: () => [],
    });
    await expect(stuck.pass()).rejects.toThrow(/cursor did not advance/);
  });

  it("reads the generation a transfer names", () => {
    const enc = new TextEncoder();
    const netstring = (text: string) => enc.encode(`${enc.encode(text).length}:${text},`);
    expect(transferGeneration(netstring("gwar archive transfer v1\nacc\n7\nrest"))).toBe(7);
    expect(transferGeneration(netstring("gwar archive manifest v1\nacc\n7"))).toBeNull();
    expect(transferGeneration(enc.encode("99:short"))).toBeNull();
    expect(transferGeneration(enc.encode("garbage"))).toBeNull();
  });
});
