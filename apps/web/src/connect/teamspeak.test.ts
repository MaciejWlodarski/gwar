import { describe, expect, it } from "vitest";
import { createAccount } from "./account";
import { changeIdentities, refreshFromVault, syncAfterUnlock, type TsBridge, type TsListInfo } from "./teamspeak";
import { addEntry, deleteEntry, emptyList, renameEntry, type TsEntry, type TsList } from "./ts-list";
import { serverVault, setup, writeServerVault } from "./testing";
import { vaultList } from "./vault";

const entry = (uid: string, name = uid): TsEntry => ({ uid, name, identity: `1V${uid}` });
const list = (names: string[], def: string | null = names[0] ?? null): TsList => ({ default: def, identities: names.map((n) => entry(n)) });
const stored = (l: TsList, now: number) => ({
  identities: l.identities.map((e) => ({ ...e, updated_at: now })),
  default: l.default,
});

/** A desktop whose identity lists are two variables. */
class FakeDesktop implements TsBridge {
  device: TsList = list(["own1", "own2"]);
  account: TsList | null = null;
  calls: string[] = [];
  made = 0;
  /** Runs inside `list("device")`, e.g. another desktop writing the vault meanwhile. */
  onList: (() => Promise<void>) | null = null;
  async list(which: "active" | "device"): Promise<TsListInfo> {
    this.calls.push(`list:${which}`);
    await this.onList?.();
    const account = which === "active" ? this.account : null;
    const used = account ?? this.device;
    return { source: account ? "account" : "device", default: used.default, identities: used.identities.map((e) => ({ ...e, level: 8 })) };
  }
  async parse(): Promise<never> {
    throw new Error("not used");
  }
  async generate() {
    this.made++;
    return { identity: `1Vmade${this.made}`, uid: `made${this.made}`, level: 8 };
  }
  async setList(which: "account" | "device", next: TsList | null) {
    this.calls.push(`setList:${which}:${next ? next.identities.map((e) => e.uid).join(",") + `;${next.default}` : "null"}`);
    if (which === "device") this.device = next!;
    else this.account = next;
  }
  async detect() {
    return [];
  }
}

async function signedIn(handle: string) {
  const s = setup();
  const { record } = await createAccount({ handle, password: "right password" }, s.deps());
  return { ...s, record, api: s.deps().api, ts: new FakeDesktop() };
}

describe("syncAfterUnlock", () => {
  it("the first desktop seeds the vault with its whole list and keeps using it", async () => {
    const { api, record, service, ts } = await signedIn("ts.first");
    ts.device = list(["own1", "own2"], "own2");
    await writeServerVault(service, record, { other: "kept" });
    expect(await syncAfterUnlock(api, record, ts, 1234)).toBe("seeded");
    expect((await serverVault(service, record))?.contents).toEqual({ other: "kept", teamspeak: stored(ts.device, 1234) });
    expect(ts.calls).toEqual(["list:device", "setList:account:own1,own2;own2"]);
  }, 30_000);

  it("a device without identities makes one named Default first, then seeds with it", async () => {
    const { api, record, service, ts } = await signedIn("ts.empty");
    ts.device = emptyList();
    expect(await syncAfterUnlock(api, record, ts, 5)).toBe("seeded");
    const expected = { default: "made1", identities: [{ uid: "made1", name: "Default", identity: "1Vmade1" }] };
    expect(ts.device).toEqual(expected);
    expect(ts.account).toEqual(expected);
    expect(vaultList((await serverVault(service, record))!.contents as never)).toEqual(expected);
    expect(ts.calls).toEqual(["list:device", "setList:device:made1;made1", "setList:account:made1;made1"]);
  }, 30_000);

  it("a later desktop adopts the vault's list without touching its own", async () => {
    const { api, record, service, ts } = await signedIn("ts.later");
    const shared = list(["acc1", "acc2"], "acc2");
    await writeServerVault(service, record, { teamspeak: stored(shared, 5) });
    expect(await syncAfterUnlock(api, record, ts)).toBe("adopted");
    expect(ts.calls).toEqual(["setList:account:acc1,acc2;acc2"]);
    expect(ts.device).toEqual(list(["own1", "own2"]));
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);

  it("adopts the older single-identity vault form as one entry named TeamSpeak", async () => {
    const { api, record, service, ts } = await signedIn("ts.legacy");
    await writeServerVault(service, record, { teamspeak: { identity: "22Vaccount", uid: "account-uid", updated_at: 5 } });
    expect(await syncAfterUnlock(api, record, ts)).toBe("adopted");
    expect(ts.account).toEqual({ default: "account-uid", identities: [{ uid: "account-uid", name: "TeamSpeak", identity: "22Vaccount" }] });
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);

  it("if another desktop seeds first, its list wins", async () => {
    const { api, record, service, ts } = await signedIn("ts.race");
    const theirs = list(["theirs"]);
    ts.onList = () => writeServerVault(service, record, { teamspeak: stored(theirs, 5) });
    expect(await syncAfterUnlock(api, record, ts)).toBe("adopted");
    expect(ts.account).toEqual(theirs);
    expect((await serverVault(service, record))?.contents).toEqual({ teamspeak: stored(theirs, 5) });
  }, 30_000);

  it("does nothing on a record without a vault key", async () => {
    const { api, record, service, ts } = await signedIn("ts.locked");
    expect(await syncAfterUnlock(api, { ...record, vaultKey: undefined }, ts)).toBe("locked");
    expect(ts.calls).toEqual([]);
    expect(await serverVault(service, record)).toBeNull();
  }, 30_000);
});

describe("refreshFromVault", () => {
  it("applies a different list, leaves a matching one alone, and never seeds", async () => {
    const { api, record, service, ts } = await signedIn("ts.refresh");
    expect(await refreshFromVault(api, record, ts)).toBe("empty");
    expect(await serverVault(service, record)).toBeNull();
    expect(ts.calls).toEqual([]);

    await writeServerVault(service, record, { teamspeak: stored(list(["a", "b"]), 5) });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
    expect(ts.account).toEqual(list(["a", "b"]));
    expect(await refreshFromVault(api, record, ts)).toBe("unchanged");
    expect(ts.calls.filter((c) => c.startsWith("setList"))).toHaveLength(1);

    await writeServerVault(service, record, { teamspeak: stored(renameEntry(list(["a", "b"]), "a", "Renamed"), 9) });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
    expect(ts.account?.identities[0]?.name).toBe("Renamed");
    await writeServerVault(service, record, { teamspeak: stored(list(["a", "b"], "b"), 9) });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
    expect(ts.account?.default).toBe("b");
  }, 30_000);

  it("the device's own list does not count as the account's", async () => {
    const { api, record, service, ts } = await signedIn("ts.same");
    await writeServerVault(service, record, { teamspeak: stored(ts.device, 1) });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
  }, 30_000);

  it("rejects on a service error so the caller can log it", async () => {
    const { api, record, service, ts } = await signedIn("ts.offline");
    service.sessions.clear();
    await expect(refreshFromVault(api, record, ts)).rejects.toMatchObject({ kind: "unauthorized" });
    expect(ts.calls).toEqual([]);
    expect(await refreshFromVault(api, { ...record, vaultKey: undefined }, ts)).toBe("locked");
  }, 30_000);
});

describe("changeIdentities", () => {
  const add = (e: TsEntry) => (l: TsList) => addEntry(l, e).list;

  it("signed in: starts from the vault's list, writes the vault first, then this desktop", async () => {
    const { api, record, service, ts } = await signedIn("ts.change");
    await writeServerVault(service, record, { teamspeak: stored(list(["a", "b"], "b"), 5), other: 1 });
    const next = await changeIdentities(api, record, ts, add(entry("c", "Third")), 77);
    expect(next).toEqual({ default: "b", identities: [entry("a"), entry("b"), entry("c", "Third")] });
    expect((await serverVault(service, record))?.contents).toEqual({
      other: 1,
      teamspeak: {
        identities: [
          { ...entry("a"), updated_at: 5 },
          { ...entry("b"), updated_at: 5 },
          { ...entry("c", "Third"), updated_at: 77 },
        ],
        default: "b",
      },
    });
    expect(ts.calls).toEqual(["setList:account:a,b,c;b"]);
    expect(ts.device).toEqual(list(["own1", "own2"]));
  }, 30_000);

  it("signed in: rename, delete of the default and the older vault form", async () => {
    const { api, record, service, ts } = await signedIn("ts.rules");
    await writeServerVault(service, record, { teamspeak: stored(list(["a", "b", "c"], "a"), 5) });
    await changeIdentities(api, record, ts, (l) => renameEntry(l, "b", "Work"), 10);
    await changeIdentities(api, record, ts, (l) => deleteEntry(l, "a"), 11);
    expect(ts.account).toEqual({ default: "b", identities: [entry("b", "Work"), entry("c")] });
    expect(vaultList((await serverVault(service, record))!.contents as never)).toEqual(ts.account);

    await writeServerVault(service, record, { teamspeak: { identity: "22Vold", uid: "old-uid", updated_at: 1 } });
    await changeIdentities(api, record, ts, add(entry("new")), 12);
    expect((await serverVault(service, record))?.contents).toEqual({
      teamspeak: {
        identities: [
          { uid: "old-uid", name: "TeamSpeak", identity: "22Vold", updated_at: 12 },
          { ...entry("new"), updated_at: 12 },
        ],
        default: "old-uid",
      },
    });
  }, 30_000);

  it("signed in: a failed vault write leaves the desktop alone", async () => {
    const { api, record, service, ts } = await signedIn("ts.fail");
    service.sessions.clear();
    await expect(changeIdentities(api, record, ts, add(entry("c")), 1)).rejects.toMatchObject({ kind: "unauthorized" });
    expect(ts.calls).toEqual([]);
  }, 30_000);

  it("signed in: no vault key is refused without touching anything", async () => {
    const { api, record, ts } = await signedIn("ts.nokey");
    await expect(changeIdentities(api, { ...record, vaultKey: undefined }, ts, add(entry("c")))).rejects.toThrow(/vault key/);
    expect(ts.calls).toEqual([]);
  }, 30_000);

  it("signed out: only this device's own list changes", async () => {
    const { api, ts } = await signedIn("ts.device");
    const next = await changeIdentities(api, undefined, ts, (l) => deleteEntry(add(entry("c"))(l), "own1"));
    expect(next).toEqual({ default: "own2", identities: [entry("own2"), entry("c")] });
    expect(ts.calls).toEqual(["list:device", "setList:device:own2,c;own2"]);
    expect(ts.account).toBeNull();
  }, 30_000);
});
