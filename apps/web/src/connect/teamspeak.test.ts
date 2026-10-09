import { describe, expect, it } from "vitest";
import { createAccount } from "./account";
import { refreshFromVault, replaceIdentity, syncAfterUnlock, vaultIdentity, type TsBridge, type TsIdentityInfo } from "./teamspeak";
import { serverVault, setup, writeServerVault } from "./testing";

const OWN = { identity: "11Vown", uid: "own-uid" };
const ACCOUNT_TS = { identity: "22Vaccount", uid: "account-uid", updated_at: 5 };

/** A desktop whose identity files are two variables. */
class FakeDesktop implements TsBridge {
  device = OWN;
  account: { identity: string; uid: string } | null = null;
  calls: string[] = [];
  /** Runs inside `export("device")`, e.g. another desktop writing the vault meanwhile. */
  onExport: (() => Promise<void>) | null = null;
  async info(): Promise<TsIdentityInfo> {
    const used = this.account ?? this.device;
    return { uid: used.uid, level: 8, source: this.account ? "account" : "device", device_uid: this.device.uid };
  }
  async export(which: "device" | "account") {
    this.calls.push(`export:${which}`);
    await this.onExport?.();
    return which === "device" ? this.device : this.account!;
  }
  async parse(): Promise<never> {
    throw new Error("not used");
  }
  async setAccount(identity: string | null) {
    this.calls.push(`setAccount:${identity}`);
    this.account = identity ? { identity, uid: identity === ACCOUNT_TS.identity ? ACCOUNT_TS.uid : `uid-of-${identity}` } : null;
  }
  async setDevice(identity: string) {
    this.calls.push(`setDevice:${identity}`);
    this.device = { identity, uid: `uid-of-${identity}` };
  }
}

async function signedIn(handle: string) {
  const s = setup();
  const { record } = await createAccount({ handle, password: "right password" }, s.deps());
  return { ...s, record, api: s.deps().api, ts: new FakeDesktop() };
}

describe("syncAfterUnlock", () => {
  it("the first desktop seeds the vault with its own identity and keeps using it", async () => {
    const { api, record, service, ts } = await signedIn("ts.first");
    await writeServerVault(service, record, { other: "kept" });
    expect(await syncAfterUnlock(api, record, ts, 1234)).toBe("seeded");
    expect((await serverVault(service, record))?.contents).toEqual({ other: "kept", teamspeak: { ...OWN, updated_at: 1234 } });
    expect(ts.calls).toEqual(["export:device", `setAccount:${OWN.identity}`]);
  }, 30_000);

  it("a later desktop adopts the vault's identity without exporting its own", async () => {
    const { api, record, service, ts } = await signedIn("ts.later");
    await writeServerVault(service, record, { teamspeak: ACCOUNT_TS });
    expect(await syncAfterUnlock(api, record, ts)).toBe("adopted");
    expect(ts.calls).toEqual([`setAccount:${ACCOUNT_TS.identity}`]);
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);

  it("if another desktop seeds first, its identity wins", async () => {
    const { api, record, service, ts } = await signedIn("ts.race");
    ts.onExport = () => writeServerVault(service, record, { teamspeak: ACCOUNT_TS });
    expect(await syncAfterUnlock(api, record, ts)).toBe("adopted");
    expect((await serverVault(service, record))?.contents).toEqual({ teamspeak: ACCOUNT_TS });
    expect(ts.calls).toEqual(["export:device", `setAccount:${ACCOUNT_TS.identity}`]);
  }, 30_000);

  it("does nothing on a record without a vault key", async () => {
    const { api, record, service, ts } = await signedIn("ts.locked");
    expect(await syncAfterUnlock(api, { ...record, vaultKey: undefined }, ts)).toBe("locked");
    expect(ts.calls).toEqual([]);
    expect(await serverVault(service, record)).toBeNull();
  }, 30_000);

  it("treats a vault entry without a usable identity as empty", () => {
    expect(vaultIdentity({})).toBeNull();
    expect(vaultIdentity({ teamspeak: { identity: "", uid: "u", updated_at: 1 } })).toBeNull();
    expect(vaultIdentity({ teamspeak: { identity: 5 as unknown as string, uid: "u", updated_at: 1 } })).toBeNull();
    expect(vaultIdentity({ teamspeak: ACCOUNT_TS })).toEqual({ identity: ACCOUNT_TS.identity, uid: ACCOUNT_TS.uid });
  });
});

describe("refreshFromVault", () => {
  it("applies a different identity, leaves a matching one alone, and never seeds", async () => {
    const { api, record, service, ts } = await signedIn("ts.refresh");
    expect(await refreshFromVault(api, record, ts)).toBe("empty");
    expect(await serverVault(service, record)).toBeNull();
    expect(ts.calls).toEqual([]);

    await writeServerVault(service, record, { teamspeak: ACCOUNT_TS });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
    expect(ts.calls).toEqual([`setAccount:${ACCOUNT_TS.identity}`]);
    expect(await refreshFromVault(api, record, ts)).toBe("unchanged");
    expect(ts.calls).toHaveLength(1);

    await writeServerVault(service, record, { teamspeak: { identity: "33Vnewer", uid: "newer-uid", updated_at: 9 } });
    expect(await refreshFromVault(api, record, ts)).toBe("applied");
    expect(ts.account?.identity).toBe("33Vnewer");
  }, 30_000);

  it("the device's own identity does not count as the account's", async () => {
    const { api, record, service, ts } = await signedIn("ts.same");
    await writeServerVault(service, record, { teamspeak: { identity: OWN.identity, uid: OWN.uid, updated_at: 1 } });
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

describe("replaceIdentity", () => {
  const next = { identity: "44Vimported", uid: "imported-uid" };

  it("signed in: the vault first, then this desktop", async () => {
    const { api, record, service, ts } = await signedIn("ts.import");
    await writeServerVault(service, record, { teamspeak: ACCOUNT_TS, other: 1 });
    await replaceIdentity(api, record, ts, next, 77);
    expect((await serverVault(service, record))?.contents).toEqual({ other: 1, teamspeak: { ...next, updated_at: 77 } });
    expect(ts.calls).toEqual([`setAccount:${next.identity}`]);
  }, 30_000);

  it("signed in: a failed vault write leaves the desktop alone", async () => {
    const { api, record, service, ts } = await signedIn("ts.fail");
    service.sessions.clear();
    await expect(replaceIdentity(api, record, ts, next)).rejects.toMatchObject({ kind: "unauthorized" });
    expect(ts.calls).toEqual([]);
  }, 30_000);

  it("signed out: only this device's own identity changes", async () => {
    const { api, ts } = await signedIn("ts.device");
    await replaceIdentity(api, undefined, ts, next);
    expect(ts.calls).toEqual([`setDevice:${next.identity}`]);
  }, 30_000);
});
