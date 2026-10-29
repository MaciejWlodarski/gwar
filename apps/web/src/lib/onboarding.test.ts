import { describe, expect, it } from "vitest";
import { identityNudgeDue, readOnboarding, type Onboarding } from "./onboarding";

const day = 86_400_000;
const state: Onboarding = { completed: true, firstUsedAt: 100, nudgeDismissed: false, roleReceived: false };
describe("identity reminder", () => {
  it("waits three days, but only one on Safari", () => {
    expect(identityNudgeDue(state, false, 100 + 3 * day - 1, "Chrome/1 Safari/1")).toBe(false);
    expect(identityNudgeDue(state, false, 100 + 3 * day, "Chrome/1 Safari/1")).toBe(true);
    expect(identityNudgeDue(state, false, 100 + day, "Version/1 Safari/1")).toBe(true);
    expect(identityNudgeDue(state, false, 100 + day, "CriOS/1 Safari/1")).toBe(false);
  });
  it("appears immediately after a role, but never after dismissal/backup, while signed in or before onboarding", () => {
    const role = { ...state, roleReceived: true };
    expect(identityNudgeDue(role, false, 101, "")).toBe(true);
    expect(identityNudgeDue({ ...role, nudgeDismissed: true }, false, 100 + 10 * day, "")).toBe(false);
    expect(identityNudgeDue(role, true, 101, "")).toBe(false);
    expect(identityNudgeDue({ ...role, completed: false }, false, 101, "")).toBe(false);
  });
  it("opens old/malformed preferences safely without changing existing identity data", () => {
    expect(readOnboarding(undefined)).toBeNull();
    expect(readOnboarding({ firstUsedAt: NaN })).toBeNull();
    expect(readOnboarding({ firstUsedAt: 100 })).toEqual({ ...state, completed: false });
    expect(readOnboarding(state)).toEqual(state);
  });
});
