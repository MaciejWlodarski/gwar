import { describe, expect, it } from "vitest";
import { createAccount } from "./account";
import { ConnectApi } from "./api";
import { setup, serverVault, writeServerVault, type FakeService } from "./testing";
import type { TsList } from "./ts-list";
import { loadVault, updateVault, vaultList, withTeamspeakList, type VaultContents } from "./vault";

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
  return new ConnectApi("https://c", fetchFn as typeof fetch);
}

describe("vault", () => {
  it("is {} before the first write", async () => {
    const { api, record } = await account("vault.empty");
    expect(await loadVault(api, record)).toEqual({ contents: {}, version: 0 });
  }, 30_000);

  it("round-trips through the service as ciphertext and keeps fields it does not know", async () => {
    const { api, record, service } = await account("vault.merge");
    await writeServerVault(service, record, {
      future: { a: [1, 2] },
      teamspeak: { identities: [{ uid: "u0", name: "Old", identity: "1Vold", updated_at: 1, extra: true }], default: "u0", note: "kept" },
    });
    const next: TsList = {
      default: "u1",
      identities: [
        { uid: "u0", name: "Old", identity: "1Vold" },
        { uid: "u1", name: "New", identity: "5Vnew" },
      ],
    };
    const state = await updateVault(api, record, (c) => withTeamspeakList(c, next, 99));
    expect(state.version).toBe(2);
    const stored = await serverVault(service, record);
    expect(stored).toEqual({
      version: 2,
      contents: {
        future: { a: [1, 2] },
        teamspeak: {
          identities: [
            { uid: "u0", name: "Old", identity: "1Vold", updated_at: 1, extra: true },
            { uid: "u1", name: "New", identity: "5Vnew", updated_at: 99 },
          ],
          default: "u1",
          note: "kept",
        },
      },
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

  it("refuses a wrong vault key without writing", async () => {
    const { api, record, service } = await account("vault.keys");
    await writeServerVault(service, record, { a: 1 });
    const wrong = { ...record, vaultKey: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" };
    await expect(updateVault(api, wrong, (c) => ({ ...c, b: 1 }))).rejects.toThrow(/decrypt/);
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);
});

describe("TeamSpeak identities in the vault", () => {
  const list: TsList = {
    default: "b",
    identities: [
      { uid: "a", name: "Main", identity: "1Va" },
      { uid: "b", name: "Alt", identity: "2Vb" },
    ],
  };

  it("reads the list form, skipping broken and repeated entries and repairing the default", () => {
    const stored = {
      teamspeak: {
        identities: [
          { uid: "a", name: "Main", identity: "1Va", updated_at: 1 },
          null,
          { uid: "a", name: "Dup", identity: "9Vdup", updated_at: 2 },
          { uid: "c", identity: "3Vc", updated_at: 3 },
          { uid: 5, name: "Bad", identity: "1V" },
          { uid: "d", name: "No key" },
        ],
        default: "gone",
      },
    };
    expect(vaultList({}).identities).toEqual([]);
    expect(vaultList(stored as VaultContents)).toEqual({
      default: "a",
      identities: [
        { uid: "a", name: "Main", identity: "1Va" },
        { uid: "c", name: "Default", identity: "3Vc" },
      ],
    });
  });

  it("writes the list, keeping everything else", () => {
    const before = { other: { deep: [1] }, teamspeak: { unknown: "kept" } };
    const written = withTeamspeakList(before, list, 50);
    expect(written).toEqual({
      other: { deep: [1] },
      teamspeak: {
        unknown: "kept",
        identities: [
          { uid: "a", name: "Main", identity: "1Va", updated_at: 50 },
          { uid: "b", name: "Alt", identity: "2Vb", updated_at: 50 },
        ],
        default: "b",
      },
    });
    expect(vaultList(written)).toEqual(list);
  });

  it("entries keep updated_at and unknown fields unless their name or key changed", () => {
    const stored = withTeamspeakList({}, list, 10);
    (stored.teamspeak!.identities![0] as Record<string, unknown>).label = "future";
    const renamed: TsList = { ...list, identities: [{ ...list.identities[0]!, name: "Renamed" }, list.identities[1]!] };
    const next = withTeamspeakList(stored, renamed, 20);
    expect(next.teamspeak!.identities).toEqual([
      { uid: "a", name: "Renamed", identity: "1Va", updated_at: 20, label: "future" },
      { uid: "b", name: "Alt", identity: "2Vb", updated_at: 10 },
    ]);
    // Writing the same list again changes nothing, so no vault write is needed.
    expect(JSON.stringify(withTeamspeakList(next, vaultList(next), 99))).toBe(JSON.stringify(next));
  });

  it("an empty list leaves no default behind", () => {
    const written = withTeamspeakList(withTeamspeakList({}, list, 1), { default: null, identities: [] }, 2);
    expect(written).toEqual({ teamspeak: { identities: [] } });
  });
});
