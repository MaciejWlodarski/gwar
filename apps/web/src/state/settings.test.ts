import { afterEach, expect, it, vi } from "vitest";

afterEach(() => vi.unstubAllGlobals());

it("drops bookmark nicknames without losing bookmarks, credentials, identity choices or the legacy nickname", async () => {
  const before = {
    version: 1,
    state: {
      lastAddress: "voice.example.com",
      lastKind: "vc",
      bookmarks: [
        { id: "gwar", name: "Gwar", address: "voice.example.com", kind: "vc", nickname: "  Łucja  ", password: "secret" },
        { id: "ts", name: "TS", address: "ts.example.com", kind: "teamspeak", nickname: "Other", identity: "ts-uid" },
      ],
    },
  };
  const storage = new Map([["vc.settings", JSON.stringify(before)]]);
  const browserStorage = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value), removeItem: (key: string) => storage.delete(key) };
  vi.stubGlobal("localStorage", browserStorage);
  vi.stubGlobal("window", { localStorage: browserStorage });
  vi.resetModules();
  const { useSettings } = await import("./settings");
  expect(useSettings.getState().lastNickname).toBe("Łucja");
  expect(useSettings.getState().bookmarks).toEqual(before.state.bookmarks.map(({ nickname: _nickname, ...bookmark }) => bookmark));
  const saved = JSON.parse(storage.get("vc.settings")!);
  expect(saved.state.bookmarks).toEqual(useSettings.getState().bookmarks);
  expect(saved.state.lastNickname).toBe("Łucja");
  const { legacyNickname } = await import("../net/nickname");
  expect(legacyNickname()).toBe("Łucja");
  useSettings.getState().setLast("new.example.com", "vc");
  expect(useSettings.getState().lastNickname).toBe("Łucja");
});

it("times only synced changes, remembers removals and forgets them when the server is saved again", async () => {
  const storage = new Map<string, string>();
  const browserStorage = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value), removeItem: (key: string) => storage.delete(key) };
  vi.stubGlobal("localStorage", browserStorage);
  vi.stubGlobal("window", { localStorage: browserStorage });
  vi.resetModules();
  vi.useFakeTimers({ now: 1_000 });
  try {
    const { useSettings } = await import("./settings");
    const s = () => useSettings.getState();
    s().saveBookmark({ id: "a", name: "A", address: "a.example.com", kind: "vc" });
    expect(s().bookmarks[0]?.updatedAt).toBe(1_000);
    expect(s().bookmarkEdits).toBe(1);

    vi.setSystemTime(2_000);
    s().saveBookmark({ ...s().bookmarks[0]!, insecureLan: true });
    expect(s().bookmarks[0]).toMatchObject({ updatedAt: 1_000, insecureLan: true });
    expect(s().bookmarkEdits).toBe(1);

    s().saveBookmark({ ...s().bookmarks[0]!, password: "secret" });
    expect(s().bookmarks[0]?.updatedAt).toBe(2_000);
    expect(s().bookmarkEdits).toBe(2);

    vi.setSystemTime(3_000);
    s().removeBookmark("a");
    expect(s().bookmarks).toEqual([]);
    expect(s().removedBookmarks).toEqual({ a: { at: 3_000 } });

    s().replaceBookmarks([{ id: "b", name: "B", address: "b.example.com", kind: "vc", updatedAt: 5 }]);
    expect(s().bookmarkEdits).toBe(3);
    s().saveBookmark({ id: "a", name: "A", address: "a.example.com", kind: "vc" });
    expect(s().removedBookmarks).toEqual({});
    expect(s().bookmarks.map((b) => b.id)).toEqual(["b", "a"]);

    // A synced bookmark from a device whose clock is ahead: edits and removals here still come after it.
    s().replaceBookmarks([{ id: "c", name: "C", address: "c.example.com", kind: "vc", updatedAt: 9_000, account: "acct" }]);
    s().saveBookmark({ id: "c", name: "C2", address: "c.example.com", kind: "vc" });
    expect(s().bookmarks[0]).toMatchObject({ name: "C2", updatedAt: 9_001, account: "acct" });
    s().removeBookmark("c");
    expect(s().removedBookmarks).toEqual({ c: { at: 9_002, account: "acct" } });

    // Signed in: new bookmarks and removals belong to the account at once, before any sync.
    s().setBookmarkAccount("acct");
    s().saveBookmark({ id: "n", name: "N", address: "n.example.com" });
    expect(s().bookmarks.at(-1)?.account).toBe("acct");
    s().replaceBookmarks([
      { id: "d", name: "D", address: "d.example.com", account: "acct" },
      { id: "big", name: "Big", address: "big.example.com", password: "p".repeat(200), account: "acct" },
      { id: "e", name: "E", address: "e.example.com", account: "other" },
      { id: "f", name: "F", address: "f.example.com" },
    ]);
    s().removeBookmark("f");
    expect(s().removedBookmarks.f).toMatchObject({ account: "acct" });
    s().removeBookmark("e");
    expect(s().removedBookmarks.e).toMatchObject({ account: "other" });
    // Signing out after a full sync: what reached the vault goes; what cannot sync and the other account's stay.
    s().dropAccountBookmarks("acct");
    expect(s().bookmarks.map((b) => b.id)).toEqual(["big"]);
    expect(Object.keys(s().removedBookmarks)).toEqual(["e"]);
  } finally {
    vi.useRealTimers();
  }
});

it("keeps one bookmark per server: on load, when saving, when editing and after syncing", async () => {
  const before = {
    version: 1,
    state: {
      bookmarks: [
        { id: "a", name: "Gwar", address: "gwar.app", kind: "vc" },
        { id: "b", name: "Gwar invite", address: "wss://gwar.app/ws", kind: "vc", password: "secret" },
        { id: "ts", name: "TS", address: "gwar.app", kind: "teamspeak" },
      ],
    },
  };
  const storage = new Map([["vc.settings", JSON.stringify(before)]]);
  const browserStorage = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value), removeItem: (key: string) => storage.delete(key) };
  vi.stubGlobal("localStorage", browserStorage);
  vi.stubGlobal("window", { localStorage: browserStorage });
  vi.resetModules();
  vi.useFakeTimers({ now: 1_000 });
  try {
    const { useSettings } = await import("./settings");
    const s = () => useSettings.getState();
    // A list saved before servers were matched: the first stays, with the password only the other had
    // (timed as a change, so syncing keeps it), and the other is removed so syncing removes it elsewhere
    // too. Another kind is another server.
    expect(s().bookmarks).toEqual([{ id: "a", name: "Gwar", address: "gwar.app", kind: "vc", password: "secret", updatedAt: 1_000 }, before.state.bookmarks[2]]);
    expect(s().removedBookmarks).toEqual({ b: { at: 1_000 } });
    expect(s().bookmarkEdits).toBe(1);

    // Saving a saved server again (another spelling) updates that bookmark in its place, keeping what
    // the new entry leaves unsaid: its address and its password.
    s().saveBookmark({ id: "new", name: "Renamed", address: "GWAR.app:443", kind: "vc" });
    expect(s().bookmarks.map((b) => [b.id, b.name, b.address, b.password])).toEqual([["a", "Renamed", "gwar.app", "secret"], ["ts", "TS", "gwar.app", undefined]]);
    // A name that is just the address is no name.
    s().saveBookmark({ id: "new2", name: "gwar.app", address: "gwar.app", kind: "vc" });
    expect(s().bookmarks[0]?.name).toBe("Renamed");

    // Editing a bookmark into another saved server replaces that one.
    s().saveBookmark({ id: "c", name: "C", address: "c.example.com", kind: "vc" });
    vi.setSystemTime(2_000);
    s().saveBookmark({ ...s().bookmarks.find((b) => b.id === "c")!, address: "wss://gwar.app/ws" });
    expect(s().bookmarks.map((b) => b.id)).toEqual(["ts", "c"]);
    expect(s().removedBookmarks.a).toEqual({ at: 2_000 });

    // Two devices saved one server under different ids: after syncing, the first in the vault's order stays.
    s().setBookmarkAccount("acct");
    vi.setSystemTime(3_000);
    s().replaceBookmarks([
      { id: "x", name: "X", address: "x.example.com", account: "acct", updatedAt: 10 },
      { id: "y", name: "X", address: "wss://x.example.com/ws", password: "pw", account: "acct", updatedAt: 20 },
      // Another account's bookmark is not shown with these and stays for its own account.
      { id: "z", name: "X", address: "x.example.com", account: "other", updatedAt: 30 },
    ]);
    expect(s().bookmarks.map((b) => b.id)).toEqual(["x", "z"]);
    expect(s().removedBookmarks.y).toEqual({ at: 3_000, account: "acct" });
    // The password it took is newer than the vault's copy of it, so the next sync keeps it.
    expect(s().bookmarks[0]).toMatchObject({ password: "pw", updatedAt: 3_000 });

    // Signed out, another account's hidden bookmark for the same server is left alone, password and all.
    s().setBookmarkAccount(null);
    s().replaceBookmarks([
      { id: "hidden", name: "Q", address: "q.example.com", password: "theirs", account: "other", updatedAt: 5 },
    ]);
    s().saveBookmark({ id: "mine", name: "Q", address: "wss://q.example.com/ws", kind: "vc" });
    expect(s().bookmarks.map((b) => [b.id, b.password])).toEqual([["hidden", "theirs"], ["mine", undefined]]);
    expect(s().removedBookmarks.hidden).toBeUndefined();
  } finally {
    vi.useRealTimers();
  }
});
