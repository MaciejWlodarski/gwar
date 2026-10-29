// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
const saved = vi.hoisted(() => ({ identity: undefined as unknown }));
vi.mock("../net/identity", async (original) => ({ ...await original<typeof import("../net/identity")>(),
  indexedDbIdentityStore: { load: vi.fn(async () => saved.identity) },
  loadConnectRecord: vi.fn(async () => undefined),
}));
vi.mock("./controller", () => ({ controller: { getIdentity: vi.fn(async () => { saved.identity ??= { nickname: "gwar-12345" }; }) }, identityFailure: () => "identity_storage" }));
import { accountActions } from "./account";
import { useSettings } from "./settings";
beforeEach(() => { saved.identity = undefined; useSettings.setState({ onboarding: null }); });
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
