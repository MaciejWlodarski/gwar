import { describe, expect, it } from "vitest";
import { createAccount } from "./account";
import { ConnectApi } from "./api";
import { changeProfileNickname, refreshProfile, syncProfileAfterUnlock } from "./profile";
import { setup, serverVault, writeServerVault } from "./testing";

async function account() {
  const s = setup();
  const { record } = await createAccount({ handle: "profile.test", password: "right password" }, s.deps());
  return { ...s, record, api: s.deps().api };
}

describe("vault profile sync", () => {
  it("adopts the vault nickname after unlock and on startup without a write", async () => {
    const { api, record, service } = await account();
    await writeServerVault(service, record, { profile: { nickname: "Anna", updated_at: 1 } });
    expect(await syncProfileAfterUnlock(api, record, "Local")).toBe("Anna");
    expect(await refreshProfile(api, record)).toBe("Anna");
    expect((await serverVault(service, record))?.version).toBe(1);
  }, 30_000);
  it("seeds only after unlock, preserving unknown sections", async () => {
    const { api, record, service } = await account();
    await writeServerVault(service, record, { future: true, teamspeak: { default: "ts" } });
    expect(await refreshProfile(api, record)).toBeUndefined();
    expect(await syncProfileAfterUnlock(api, record, "Local", 10)).toBe("Local");
    expect((await serverVault(service, record))?.contents).toEqual({ future: true, teamspeak: { default: "ts" }, profile: { nickname: "Local", updated_at: 10 } });
  }, 30_000);
  it("adopts another device's seed after a 409 conflict", async () => {
    const { record, service } = await account();
    let puts = 0;
    const api = new ConnectApi("http://c", (async (url, init) => {
      if (init?.method === "PUT" && ++puts === 1) await writeServerVault(service, record, { profile: { nickname: "Other device", updated_at: 5 }, future: true });
      return service.fetch(url, init);
    }) as typeof fetch);
    expect(await syncProfileAfterUnlock(api, record, "Local", 10)).toBe("Other device");
    expect((await serverVault(service, record))?.contents).toMatchObject({ future: true });
  }, 30_000);
  it("retries edits on the latest vault and preserves other profile fields", async () => {
    const { record, service } = await account();
    let puts = 0;
    const api = new ConnectApi("http://c", (async (url, init) => {
      if (init?.method === "PUT" && ++puts === 1) await writeServerVault(service, record, { profile: { nickname: "Other", updated_at: 5, avatar: "kept" }, teamspeak: { default: "ts" } });
      return service.fetch(url, init);
    }) as typeof fetch);
    expect(await changeProfileNickname(api, record, "  Changed  ", 10)).toBe("Changed");
    expect(puts).toBe(2);
    expect((await serverVault(service, record))?.contents).toEqual({ profile: { nickname: "Changed", updated_at: 10, avatar: "kept" }, teamspeak: { default: "ts" } });
  }, 30_000);
  it("keeps the local nickname while locked and rejects edits without writing", async () => {
    const { api, record, service } = await account();
    const locked = { ...record, vaultKey: undefined };
    expect(await syncProfileAfterUnlock(api, locked, "Local")).toBe("Local");
    expect(await refreshProfile(api, locked)).toBeUndefined();
    await expect(changeProfileNickname(api, locked, "New")).rejects.toThrow(/vault key/);
    expect(await serverVault(service, record)).toBeNull();
  }, 30_000);
});
