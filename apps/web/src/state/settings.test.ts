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
    expect(s().removedBookmarks).toEqual({ a: 3_000 });

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
    expect(s().removedBookmarks).toEqual({ c: 9_002 });

    s().replaceBookmarks([
      { id: "d", name: "D", address: "d.example.com", account: "acct" },
      { id: "e", name: "E", address: "e.example.com", account: "other" },
      { id: "f", name: "F", address: "f.example.com" },
    ]);
    s().dropAccountBookmarks("acct");
    expect(s().bookmarks.map((b) => b.id)).toEqual(["e", "f"]);
    expect(s().removedBookmarks).toEqual({});
  } finally {
    vi.useRealTimers();
  }
});
