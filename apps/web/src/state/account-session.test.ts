// @vitest-environment happy-dom
import { afterEach, expect, it, vi } from "vitest";
import * as identity from "../net/identity";
import * as flows from "../connect/account";
import * as servers from "../connect/servers";
import { accountActions, useAccount } from "./account";
import { useSettings } from "./settings";

// The local fixtures must never ask the public account service for profile/key data.
vi.mock("../lib/official", async get => ({ ...await get<typeof import("../lib/official")>(), CONNECT_URL: "http://127.0.0.1:18980" }));

afterEach(() => vi.restoreAllMocks());
it.each([false, true])("sign-out clears server secrets and update voice intent before account I/O (failure=%s)", async failure => {
  const record = { accountKey: "old-account", token: "local-test" } as identity.ConnectRecord;
  vi.spyOn(identity, "loadConnectRecord").mockResolvedValue(record);
  vi.spyOn(servers, "syncServers").mockResolvedValue([]);
  const signOut = vi.spyOn(flows, "signOut").mockImplementation(async () => {
    expect(useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
    if (failure) throw new Error("sign-out interrupted");
  });
  useSettings.setState({
    resumeConnection: { kind: "teamspeak", address: "old.example", name: "Old", password: "secret", identity: "old-ts-id" },
    updateVoiceResume: { kind: "teamspeak", address: "old.example", channel: 1, expiresAt: Date.now() + 1000, version: __APP_VERSION__ },
  });
  if (failure) await expect(accountActions.signOut()).rejects.toThrow("sign-out interrupted");
  else { await accountActions.signOut(); expect(useAccount.getState().account).toBeNull(); }
  expect(signOut).toHaveBeenCalledOnce();
  expect(useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
});
