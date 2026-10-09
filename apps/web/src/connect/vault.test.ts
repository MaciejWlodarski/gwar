import { describe, expect, it } from "vitest";
import { createAccount } from "./account";
import { ConnectApi } from "./api";
import { setup, serverVault, writeServerVault, type FakeService } from "./testing";
import { loadVault, updateVault, VaultLockedError, withTeamspeak } from "./vault";

async function account(handle: string) {
  const s = setup();
  const { record } = await createAccount({ handle, password: "right password" }, s.deps());
  return { ...s, record, api: s.deps().api };
}

/** An API whose PUTs are preceded by `before(n)` (n counts PUTs from 1), e.g. another device writing first. */
function racing(service: FakeService, before: (n: number) => Promise<void>): ConnectApi {
  let puts = 0;
  const fetchFn = async (url: string | URL | Request, init?: RequestInit) => {
    if (init?.method === "PUT" && String(url).endsWith("/vault")) await before(++puts);
    return service.fetch(url, init);
  };
  return new ConnectApi("http://c", fetchFn as typeof fetch);
}

describe("vault", () => {
  it("is {} before the first write", async () => {
    const { api, record } = await account("vault.empty");
    expect(await loadVault(api, record)).toEqual({ contents: {}, version: 0 });
  }, 30_000);

  it("round-trips through the service as ciphertext and keeps fields it does not know", async () => {
    const { api, record, service } = await account("vault.merge");
    await writeServerVault(service, record, { future: { a: [1, 2] }, teamspeak: { identity: "old", uid: "u0", updated_at: 1, extra: true } });
    const state = await updateVault(api, record, (c) => withTeamspeak(c, "5Vnew", "u1", 99));
    expect(state.version).toBe(2);
    const stored = await serverVault(service, record);
    expect(stored).toEqual({
      version: 2,
      contents: { future: { a: [1, 2] }, teamspeak: { identity: "5Vnew", uid: "u1", updated_at: 99, extra: true } },
    });
    expect(service.vaults.get(record.handle)!.vault).not.toContain("5Vnew");
    expect(await loadVault(api, record)).toEqual({ contents: stored!.contents, version: 2 });
  }, 30_000);

  it("writes on top of the version it read, and not at all when nothing changes", async () => {
    const { api, record, service } = await account("vault.version");
    await writeServerVault(service, record, { a: 1 });
    await writeServerVault(service, record, { a: 2 });
    await updateVault(api, record, (c) => ({ ...c, b: 3 }));
    expect((await serverVault(service, record))?.version).toBe(3);
    await updateVault(api, record, (c) => c);
    expect((await serverVault(service, record))?.version).toBe(3);
  }, 30_000);

  it("on a conflict fetches again, re-applies the change and retries", async () => {
    const { record, service } = await account("vault.race");
    const api = racing(service, async (n) => {
      if (n === 1) await writeServerVault(service, record, { fromOther: true });
    });
    let calls = 0;
    const state = await updateVault(api, record, (c) => {
      calls++;
      return { ...c, mine: calls };
    });
    expect(calls).toBe(2);
    expect(state.contents).toEqual({ fromOther: true, mine: 2 });
    expect(await serverVault(service, record)).toEqual({ version: 2, contents: { fromOther: true, mine: 2 } });
  }, 30_000);

  it("gives up after a few conflicts", async () => {
    const { record, service } = await account("vault.storm");
    const api = racing(service, () => writeServerVault(service, record, { n: Math.random() }));
    let calls = 0;
    await expect(updateVault(api, record, (c) => ({ ...c, mine: ++calls }))).rejects.toMatchObject({ kind: "conflict" });
    expect(calls).toBe(4);
  }, 30_000);

  it("needs the vault key, and refuses a wrong one without writing", async () => {
    const { api, record, service } = await account("vault.keys");
    await writeServerVault(service, record, { a: 1 });
    await expect(loadVault(api, { ...record, vaultKey: undefined })).rejects.toBeInstanceOf(VaultLockedError);
    await expect(updateVault(api, { ...record, vaultKey: undefined }, (c) => c)).rejects.toBeInstanceOf(VaultLockedError);
    const wrong = { ...record, vaultKey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" };
    await expect(updateVault(api, wrong, (c) => ({ ...c, b: 1 }))).rejects.toThrow(/decrypt/);
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);
});
