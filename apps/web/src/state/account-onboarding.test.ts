// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { NewerDataError } from "../lib/stored-data";
import type { ConnectRecord } from "../net/identity";
import { loadConnectRecord, IdentityUnreadableError } from "../net/identity";
import { useUi } from "./stores";
const saved = vi.hoisted(() => ({ identity: undefined as unknown, record: undefined as ConnectRecord | undefined, identityError: undefined as unknown, connectError: undefined as unknown }));
vi.mock("../net/identity", async (original) => ({ ...await original<typeof import("../net/identity")>(),
  indexedDbIdentityStore: { load: vi.fn(async () => { if (saved.identityError) throw saved.identityError; return saved.identity; }) },
  loadConnectRecord: vi.fn(async () => { if (saved.connectError) throw saved.connectError; return saved.record; }),
}));
vi.mock("./controller", async () => ({ controller: { getIdentity: vi.fn(async () => { if (saved.identityError) throw saved.identityError; saved.identity ??= { nickname: "gwar-12345" }; }) }, identityFailure: (await import("../net/identity")).identityFailure }));
vi.mock("../connect/profile", () => ({ refreshProfile: vi.fn(async () => null) }));
vi.mock("./sync-queue", () => ({ syncQueue: () => ({ sync: vi.fn() }) }));
import { accountActions, useAccount } from "./account";
import { useSettings } from "./settings";
beforeEach(() => {
  saved.identity = saved.identityError = saved.connectError = undefined; saved.record = undefined;
  useSettings.setState({ onboarding: null });
  useAccount.setState({ loaded: false, account: null, certChecked: false });
  useUi.setState({ toasts: [] });
});
describe("onboarding initialization across upgrades and reloads", () => {
  it("keeps asking after the fallback has been generated and the unfinished app reloads", async () => {
    await accountActions.load();
    expect(saved.identity).toEqual({ nickname: "gwar-12345" });
    expect(useSettings.getState().onboarding?.completed).toBe(false);
    await accountActions.load();
    expect(useSettings.getState().onboarding?.completed).toBe(false);
  });
  it("skips the name step for an existing identity, including old random names, without renaming it", async () => {
    saved.identity = { nickname: "gwar-12345", jwk: "untouched" };
    await accountActions.load();
    expect(useSettings.getState().onboarding?.completed).toBe(true);
    expect(saved.identity).toEqual({ nickname: "gwar-12345", jwk: "untouched" });
  });
  it("preserves a prior skip and reminder dismissal", async () => {
    useSettings.getState().setOnboarding({ completed: true, nudgeDismissed: true });
    await accountActions.load();
    expect(useSettings.getState().onboarding).toMatchObject({ completed: true, nudgeDismissed: true });
  });
});

describe("startup storage errors", () => {
  it.each([false, true])("finishes loading a newer Connect record with an actionable error (initialized preferences: %s)", async initialized => {
    if (initialized) useSettings.getState().setOnboarding({ completed: true });
    saved.connectError = new NewerDataError("Connect", 2, 1);
    await accountActions.load();
    expect(useAccount.getState().loaded).toBe(true);
    expect(useUi.getState().toasts).toEqual([expect.objectContaining({ key: "identity-load", sticky: true, action: expect.objectContaining({ label: "Identity" }) })]);
    expect(saved.identity).toBeUndefined();
  });
  it("keeps loading a valid Connect login when the local identity cannot be read", async () => {
    saved.identityError = new IdentityUnreadableError();
    saved.record = { handle: "alex", accountKey: "account", deviceName: "Browser", certificate: { device_key: "device", expires_at: 100 }, token: "" } as ConnectRecord;
    await accountActions.load();
    expect(loadConnectRecord).toHaveBeenCalled();
    expect(useAccount.getState()).toMatchObject({ loaded: true, account: { handle: "alex", accountKey: "account", deviceKey: "device" } });
    expect(useUi.getState().toasts.some(t => t.key === "identity-load")).toBe(true);
  });
});
