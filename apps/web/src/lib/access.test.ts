import { describe, expect, it } from "vitest";
import { rebaseAccess, sameAccess } from "./access";

describe("private channel access", () => {
  it("compares regardless of order", () => {
    expect(sameAccess({ roles: [2, 1], users: ["b", "a"] }, { roles: [1, 2], users: ["a", "b"] })).toBe(true);
    expect(sameAccess(null, { roles: [], users: [] })).toBe(false);
  });
  it("applies only my changes on top of the current access", () => {
    const opened = { roles: [5], users: ["a", "b"] };
    const mine = { roles: [5, 6], users: ["a", "b", "c"] };
    // Meanwhile someone removed b and role 5, and added d.
    const current = { roles: [], users: ["a", "d"] };
    expect(rebaseAccess(opened, mine, current)).toEqual({ roles: [6], users: ["a", "d", "c"] });
    // A removal of mine still applies.
    expect(rebaseAccess(opened, { roles: [5], users: ["b"] }, current)).toEqual({ roles: [], users: ["d"] });
  });
});
