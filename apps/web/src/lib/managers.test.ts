import { describe, expect, it } from "vitest";
import type { OwnerView } from "../mls/types";
import { managerUpkeep, type UpkeepInput } from "./managers";

const DAY = 86_400_000, NOW = 1_800_000_000_000;
const view = (over: Partial<OwnerView> = {}): OwnerView => ({
  generation: 1, seq: 0, head: "h", owner: "owner", confirmers: ["conf"], managers: ["m1", "m2"],
  list_signer: "owner", list_signed_at: NOW - DAY, list_signer_expires_at: null, anchor_seq: 1, list_seq: 3, ...over,
});
const input = (over: Partial<UpkeepInput> = {}): UpkeepInput => ({
  me: "owner", view: view(), server: { anchor: "A", list: "L", pending: [], stale: [] }, current: true, settled: true,
  granted: new Set(), asked: new Set(), now: NOW, ownExpiry: Infinity, lastRenewal: 0, ...over,
});

describe("manager list upkeep (6b)", () => {
  it("is nothing for plain members and managers", () => {
    expect(managerUpkeep(input({ me: "m1", server: { anchor: "A", list: "L", pending: ["p"], stale: ["m2"] } }))).toEqual({ kind: "none" });
  });
  it("drops stale managers and adds people the signer just gave the role, consuming that approval", () => {
    const action = managerUpkeep(input({ me: "conf", server: { anchor: "A", list: "L", pending: ["new", "other"], stale: ["m2"] }, granted: new Set(["new"]) }));
    expect(action).toEqual({ kind: "publish", managers: ["m1", "new"], renewal: false, consumed: ["new"] });
  });
  it("asks about each other pending person once", () => {
    expect(managerUpkeep(input({ server: { anchor: "A", list: "L", pending: ["p"], stale: [] } }))).toEqual({ kind: "ask", uid: "p" });
    expect(managerUpkeep(input({ server: { anchor: "A", list: "L", pending: ["p"], stale: [] }, asked: new Set(["p"]) }))).toEqual({ kind: "none" });
  });
  it("renews an old, expiring or lapsed list, at most every ten minutes", () => {
    expect(managerUpkeep(input({ view: view({ list_signed_at: NOW - 61 * DAY }) })).kind).toBe("publish");
    expect(managerUpkeep(input({ view: view({ list_signer_expires_at: NOW + 3 * DAY }) })).kind).toBe("publish");
    expect(managerUpkeep(input({ current: false }))).toEqual({ kind: "publish", managers: null, renewal: true, consumed: [] });
    // Not current: no change is made against this device's possibly older checkpoint.
    expect(managerUpkeep(input({ current: false, lastRenewal: NOW - 60_000, server: { anchor: "A", list: "L", pending: [], stale: ["m2"] } }))).toEqual({ kind: "none" });
    expect(managerUpkeep(input({ current: false, lastRenewal: NOW - 60_000 }))).toEqual({ kind: "none" });
  });
  it("does not renew for expiry with a device that expires as soon", () => {
    expect(managerUpkeep(input({ view: view({ list_signer_expires_at: NOW + 3 * DAY }), ownExpiry: NOW + 2 * DAY }))).toEqual({ kind: "none" });
  });
  it("has the owner remove confirmers who lost channel_edit, once per connection", () => {
    const server = { anchor: "A", list: "L", pending: [], stale: ["conf"] };
    expect(managerUpkeep(input({ server }))).toEqual({ kind: "staleConfirmers", keep: [], stale: ["conf"] });
    expect(managerUpkeep(input({ server, asked: new Set(["@confirmers"]) })).kind).toBe("none");
    expect(managerUpkeep(input({ me: "conf", server })).kind).toBe("none");
    // A lapsed pair is renewed (keeping the server's membership) before any confirmer change.
    expect(managerUpkeep(input({ server, current: false }))).toEqual({ kind: "publish", managers: null, renewal: true, consumed: [] });
  });
  it("does nothing until the server's newest list has been checked", () => {
    const server = { anchor: "A", list: "L2", pending: ["p"], stale: ["m2", "conf"] };
    expect(managerUpkeep(input({ server, settled: false, granted: new Set(["p"]) }))).toEqual({ kind: "none" });
    expect(managerUpkeep(input({ settled: false, current: false }))).toEqual({ kind: "none" });
  });
  it("waits for the first anchor before maintaining a list", () => {
    expect(managerUpkeep(input({ server: { anchor: null, list: null, pending: ["p"], stale: [] } }))).toEqual({ kind: "none" });
  });
});
