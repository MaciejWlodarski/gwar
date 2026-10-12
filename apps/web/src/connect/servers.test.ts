import { describe, expect, it } from "vitest";
import type { Bookmark } from "../state/settings";
import { createAccount, signIn } from "./account";
import { forAccount, MAX_SERVERS, mergeServers, syncServers, TOMBSTONE_MS, toBookmarks, vaultServers, type VaultServer } from "./servers";
import { serverVault, setup, writeServerVault } from "./testing";

const NOW = 1_800_000_000_000;
const ACCT = "account-key";
/** Small times are offsets from a recent moment, so tombstones are fresh. */
const T = (n: number) => NOW - 1_000_000 + n;
const entry = (id: string, at: number, extra: Partial<VaultServer> = {}): VaultServer => ({ id, name: id, address: `${id}.example.com`, kind: "vc", updated_at: T(at), ...extra });
const bookmark = (id: string, at: number | undefined, extra: Partial<Bookmark> = {}): Bookmark => ({ id, name: id, address: `${id}.example.com`, kind: "vc", updatedAt: at === undefined ? undefined : T(at), ...extra });

describe("vaultServers", () => {
  it("keeps valid entries in order and skips malformed or repeated ones", () => {
    const entries = vaultServers({
      servers: {
        entries: [
          entry("a", 1, { future: true }),
          { id: "a", address: "dup.example.com", updated_at: 2 },
          { id: "noaddr", updated_at: 1 },
          { id: "notime", address: "x.example.com" },
          { id: "x".repeat(65), address: "long.example.com", updated_at: 1 },
          { id: "ts", address: "ts.example.com", kind: "teamspeak", identity: "uid", password: "p", updated_at: 3 },
          { id: "vcid", address: "v.example.com", identity: "uid", password: "p".repeat(129), updated_at: 3 },
          { id: "gone", updated_at: 4, deleted: true, password: "leftover" },
          null,
          [],
        ],
      },
    });
    expect(entries).toEqual([
      entry("a", 1, { future: true }),
      { id: "ts", name: "ts.example.com", address: "ts.example.com", kind: "teamspeak", identity: "uid", password: "p", updated_at: 3 },
      { id: "vcid", name: "v.example.com", address: "v.example.com", kind: "vc", updated_at: 3 },
      { id: "gone", name: "", address: "", kind: "vc", updated_at: 4, deleted: true },
    ]);
  });

  it("is empty without a servers section", () => {
    expect(vaultServers({})).toEqual([]);
    expect(vaultServers({ servers: { entries: "no" } })).toEqual([]);
  });
});

describe("mergeServers", () => {
  it("keeps the newer side per id, the vault on a tie, and its order first", () => {
    const merged = mergeServers(
      [entry("a", 5, { name: "vault a" }), entry("b", 5, { name: "vault b", extra: 1 }), entry("c", 5, { name: "vault c" })],
      { bookmarks: [bookmark("c", 5, { name: "local c" }), bookmark("b", 9, { name: "local b" }), bookmark("a", 1, { name: "local a" }), bookmark("d", 2)], removed: {} },
      NOW,
    );
    expect(merged.map((e) => [e.id, e.name])).toEqual([["a", "vault a"], ["b", "local b"], ["c", "vault c"], ["d", "d"]]);
    expect(merged[1]?.extra).toBe(1);
  });

  it("removals win only when newer, and bookmarks from before syncing lose to the vault", () => {
    const merged = mergeServers(
      [entry("old", 5), entry("fresh", 20), entry("legacy", 1, { name: "vault legacy" }), { ...entry("deleted", 30), deleted: true }],
      { bookmarks: [bookmark("legacy", undefined, { name: "local legacy" }), bookmark("deleted", 10)], removed: { old: { at: T(10) }, fresh: { at: T(10) } } },
      NOW,
    );
    expect(merged.map((e) => [e.id, !!e.deleted, e.name])).toEqual([
      ["old", true, ""],
      ["fresh", false, "fresh"],
      ["legacy", false, "vault legacy"],
      ["deleted", true, "deleted"],
    ]);
  });

  it("a newer local edit drops a password or identity the vault entry had", () => {
    const [merged] = mergeServers([entry("a", 1, { password: "old", kind: "teamspeak", identity: "u" })], { bookmarks: [bookmark("a", 2, { kind: "teamspeak" })], removed: {} }, NOW);
    expect(merged).toEqual(entry("a", 2, { kind: "teamspeak" }));
  });

  it("forgets old tombstones and caps the list, dropping tombstones before servers", () => {
    const old = { ...entry("old", 0), updated_at: NOW - TOMBSTONE_MS, deleted: true as const };
    expect(mergeServers([old], { bookmarks: [], removed: {} }, NOW)).toEqual([]);
    const many = Array.from({ length: MAX_SERVERS }, (_, i) => entry(`s${i}`, 100 + i));
    const merged = mergeServers([{ ...entry("t", 999), deleted: true }, ...many], { bookmarks: [bookmark("new", 999)], removed: {} }, NOW);
    expect(merged).toHaveLength(MAX_SERVERS);
    expect(merged.some((e) => e.id === "t")).toBe(false);
    expect(merged.some((e) => e.id === "new")).toBe(true);
    const full = mergeServers(many, { bookmarks: [bookmark("new", 999)], removed: {} }, NOW);
    expect(full.some((e) => e.id === "s0")).toBe(false);
  });
});

describe("toBookmarks", () => {
  it("keeps the LAN opt-in for an unchanged address and keeps bookmarks that cannot sync", () => {
    const previous = [
      bookmark("lan", 1, { address: "192.168.1.10:8790", insecureLan: true }),
      bookmark("moved", 1, { address: "10.0.0.2:8790", insecureLan: true }),
      bookmark("huge", 1, { password: "p".repeat(200) }),
    ];
    const result = toBookmarks([entry("lan", 2, { address: "192.168.1.10:8790" }), entry("moved", 2, { address: "10.0.0.3:8790" }), { ...entry("x", 3), deleted: true }], previous, ACCT);
    expect(result).toEqual([
      { id: "lan", name: "lan", address: "192.168.1.10:8790", kind: "vc", updatedAt: T(2), account: ACCT, insecureLan: true },
      { id: "moved", name: "moved", address: "10.0.0.3:8790", kind: "vc", updatedAt: T(2), account: ACCT },
      previous[2],
    ]);
  });

  it("a bookmark edited past the limits keeps its place and its edit over the vault's older version", () => {
    const edited = bookmark("a", 5, { password: "p".repeat(129), account: ACCT });
    expect(toBookmarks([entry("a", 1, { password: "short" }), entry("b", 1)], [edited, bookmark("b", 1, { account: ACCT })], ACCT)).toEqual([
      edited,
      { id: "b", name: "b", address: "b.example.com", kind: "vc", updatedAt: T(1), account: ACCT },
    ]);
  });

  it("syncs only the account's own and unowned bookmarks and removals, and keeps other accounts' for later", () => {
    const theirs = bookmark("theirs", 1, { account: "someone else" });
    const previous = [theirs, bookmark("unowned", 1)];
    const local = { bookmarks: previous, removed: { gone: { at: T(1), account: "someone else" }, mine: { at: T(1), account: ACCT }, any: { at: T(1) } } };
    const own = forAccount(local, ACCT);
    expect(own.bookmarks.map((b) => b.id)).toEqual(["unowned"]);
    expect(Object.keys(own.removed)).toEqual(["mine", "any"]);
    expect(toBookmarks([entry("unowned", 1)], previous, ACCT).map((b) => [b.id, b.account])).toEqual([["unowned", ACCT], ["theirs", "someone else"]]);
  });
});

describe("syncServers", () => {
  it("shares servers between devices, propagates removals and keeps other vault sections", async () => {
    const s = setup();
    const { record: laptop } = await createAccount({ handle: "servers.sync", password: "right password" }, s.deps());
    const api = s.deps().api;
    await writeServerVault(s.service, laptop, { profile: { nickname: "Dominika", updated_at: 1 } });

    const first = await syncServers(api, laptop, { bookmarks: [bookmark("gwar", 10, { name: "Gwar", address: "gwar.app", password: "secret" })], removed: {} }, NOW);
    expect(first.map((e) => e.id)).toEqual(["gwar"]);

    const phone = await signIn({ handle: "servers.sync", password: "right password" }, s.deps("Firefox on Linux"));
    const onPhone = await syncServers(api, phone, { bookmarks: [bookmark("ts", 20, { kind: "teamspeak", address: "ts.example.com", identity: "uid1" })], removed: {} }, NOW);
    expect(toBookmarks(onPhone, [], phone.accountKey)).toEqual([
      { id: "gwar", name: "Gwar", address: "gwar.app", kind: "vc", updatedAt: T(10), account: phone.accountKey, password: "secret" },
      { id: "ts", name: "ts", address: "ts.example.com", kind: "teamspeak", updatedAt: T(20), account: phone.accountKey, identity: "uid1" },
    ]);

    // The laptop removes the TeamSpeak server; the phone, which still has it, does not bring it back.
    await syncServers(api, laptop, { bookmarks: toBookmarks(onPhone, [], phone.accountKey).filter((b) => b.id !== "ts"), removed: { ts: { at: T(30), account: laptop.accountKey } } }, NOW);
    const again = await syncServers(api, phone, { bookmarks: toBookmarks(onPhone, [], phone.accountKey), removed: {} }, NOW);
    expect(toBookmarks(again, [], phone.accountKey).map((b) => b.id)).toEqual(["gwar"]);

    const stored = await serverVault(s.service, laptop);
    expect(stored?.contents).toEqual({
      profile: { nickname: "Dominika", updated_at: 1 },
      servers: { entries: [{ id: "gwar", name: "Gwar", address: "gwar.app", kind: "vc", password: "secret", updated_at: T(10) }, { id: "ts", updated_at: T(30), deleted: true }] },
    });

    // Nothing changed: nothing is written.
    const version = stored!.version;
    await syncServers(api, phone, { bookmarks: toBookmarks(again, [], phone.accountKey), removed: {} }, NOW);
    expect((await serverVault(s.service, laptop))?.version).toBe(version);
  }, 60_000);

  it("writes nothing for an account without servers, and never another account's servers", async () => {
    const s = setup();
    const { record } = await createAccount({ handle: "servers.none", password: "right password" }, s.deps());
    expect(await syncServers(s.deps().api, record, { bookmarks: [], removed: {} }, NOW)).toEqual([]);
    const theirs = { bookmarks: [bookmark("theirs", 1, { account: "someone else", password: "theirs" })], removed: { old: { at: T(1), account: "someone else" } } };
    expect(await syncServers(s.deps().api, record, theirs, NOW)).toEqual([]);
    expect(await serverVault(s.service, record)).toBeNull();
  }, 30_000);
});
