import { afterEach, expect, it, vi } from "vitest";
import { SETTINGS_VERSION } from "../lib/local-storage";

afterEach(() => vi.unstubAllGlobals());

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
    s().saveBookmark({ id: "a", name: "A", address: "a.example.com", kind: "gwar" });
    expect(s().bookmarks[0]?.updatedAt).toBe(1_000);
    expect(s().bookmarkEdits).toBe(1);

    vi.setSystemTime(2_000);
    s().saveBookmark({ ...s().bookmarks[0]!, insecureLan: true });
    expect(s().bookmarks[0]).toMatchObject({ updatedAt: 1_000, insecureLan: true });
    expect(s().bookmarkEdits).toBe(1);

    s().saveBookmark({ ...s().bookmarks[0]!, password: "secret" });
    expect(s().bookmarks[0]?.updatedAt).toBe(2_000);
    expect(s().bookmarkEdits).toBe(2);

    // A TeamSpeak server's channel and its password are synced too.
    vi.setSystemTime(2_500);
    s().saveBookmark({ id: "t", name: "T", address: "t.example.com", kind: "teamspeak" });
    vi.setSystemTime(2_600);
    s().saveBookmark({ ...s().bookmarks.find((b) => b.id === "t")!, channel: "Lobby/Games", channelPassword: "pw" });
    expect(s().bookmarks.find((b) => b.id === "t")).toMatchObject({ channel: "Lobby/Games", channelPassword: "pw", updatedAt: 2_600 });
    s().removeBookmark("t");

    vi.setSystemTime(3_000);
    s().removeBookmark("a");
    expect(s().bookmarks).toEqual([]);
    expect(s().removedBookmarks).toEqual({ a: { at: 3_000 }, t: { at: 2_601 } });

    s().replaceBookmarks([{ id: "b", name: "B", address: "b.example.com", kind: "gwar", updatedAt: 5 }]);
    expect(s().bookmarkEdits).toBe(6);
    s().saveBookmark({ id: "a", name: "A", address: "a.example.com", kind: "gwar" });
    expect(s().removedBookmarks).toEqual({ t: { at: 2_601 } });
    expect(s().bookmarks.map((b) => b.id)).toEqual(["b", "a"]);

    // A synced bookmark from a device whose clock is ahead: edits and removals here still come after it.
    s().replaceBookmarks([{ id: "c", name: "C", address: "c.example.com", kind: "gwar", updatedAt: 9_000, account: "acct" }]);
    s().saveBookmark({ id: "c", name: "C2", address: "c.example.com", kind: "gwar" });
    expect(s().bookmarks[0]).toMatchObject({ name: "C2", updatedAt: 9_001, account: "acct" });
    s().removeBookmark("c");
    expect(s().removedBookmarks).toEqual({ t: { at: 2_601 }, c: { at: 9_002, account: "acct" } });

    // Signed in: new bookmarks and removals belong to the account at once, before any sync.
    s().setBookmarkAccount("acct");
    s().saveBookmark({ id: "n", name: "N", address: "n.example.com", kind: "gwar" });
    expect(s().bookmarks.at(-1)?.account).toBe("acct");
    s().replaceBookmarks([
      { id: "d", name: "D", address: "d.example.com", kind: "gwar", account: "acct", updatedAt: 1 },
      { id: "big", name: "Big", address: "big.example.com", kind: "gwar", password: "p".repeat(200), account: "acct", updatedAt: 1 },
      { id: "e", name: "E", address: "e.example.com", kind: "gwar", account: "other", updatedAt: 1 },
      { id: "f", name: "F", address: "f.example.com", kind: "gwar", updatedAt: 1 },
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

it("keeps one bookmark per server: when saving, when editing and after syncing", async () => {
  const storage = new Map<string, string>();
  const browserStorage = { getItem: (key: string) => storage.get(key) ?? null, setItem: (key: string, value: string) => storage.set(key, value), removeItem: (key: string) => storage.delete(key) };
  vi.stubGlobal("localStorage", browserStorage);
  vi.stubGlobal("window", { localStorage: browserStorage });
  vi.resetModules();
  vi.useFakeTimers({ now: 1_000 });
  try {
    const { useSettings } = await import("./settings");
    const s = () => useSettings.getState();
    s().saveBookmark({ id: "a", name: "Gwar", address: "gwar.app", kind: "gwar", password: "secret" });
    // Another kind is another server.
    s().saveBookmark({ id: "ts", name: "TS", address: "gwar.app", kind: "teamspeak" });
    expect(s().bookmarks.map((b) => b.id)).toEqual(["a", "ts"]);

    // Saving a saved server again (another spelling) updates that bookmark in its place, keeping what
    // the new entry leaves unsaid: its address and its password.
    s().saveBookmark({ id: "new", name: "Renamed", address: "GWAR.app:443", kind: "gwar" });
    expect(s().bookmarks.map((b) => [b.id, b.name, b.address, b.password])).toEqual([["a", "Renamed", "gwar.app", "secret"], ["ts", "TS", "gwar.app", undefined]]);
    // A name that is just the address is no name.
    s().saveBookmark({ id: "new2", name: "gwar.app", address: "gwar.app", kind: "gwar" });
    expect(s().bookmarks[0]?.name).toBe("Renamed");

    // Editing a bookmark into another saved server replaces that one.
    s().saveBookmark({ id: "c", name: "C", address: "c.example.com", kind: "gwar" });
    vi.setSystemTime(2_000);
    s().saveBookmark({ ...s().bookmarks.find((b) => b.id === "c")!, address: "wss://gwar.app/ws" });
    expect(s().bookmarks.map((b) => b.id)).toEqual(["ts", "c"]);
    // After every version in the group, the edit included.
    expect(s().removedBookmarks.a).toEqual({ at: 2_001 });

    // Two devices saved one server under different ids: after syncing, the first in the vault's order stays.
    s().setBookmarkAccount("acct");
    vi.setSystemTime(3_000);
    s().replaceBookmarks([
      { id: "x", name: "X", address: "x.example.com", kind: "gwar", account: "acct", updatedAt: 10 },
      { id: "y", name: "X", address: "wss://x.example.com/ws", kind: "gwar", password: "pw", account: "acct", updatedAt: 20 },
      // Another account's bookmark is not shown with these and stays for its own account.
      { id: "z", name: "X", address: "x.example.com", kind: "gwar", account: "other", updatedAt: 30 },
    ]);
    expect(s().bookmarks.map((b) => b.id)).toEqual(["x", "z"]);
    expect(s().removedBookmarks.y).toEqual({ at: 3_000, account: "acct" });
    // The password it took is newer than the vault's copy of it, so the next sync keeps it.
    expect(s().bookmarks[0]).toMatchObject({ password: "pw", updatedAt: 3_000 });

    // Signed out, another account's hidden bookmark for the same server is left alone, password and all.
    s().setBookmarkAccount(null);
    s().replaceBookmarks([
      { id: "hidden", name: "Q", address: "q.example.com", kind: "gwar", password: "theirs", account: "other", updatedAt: 5 },
    ]);
    s().saveBookmark({ id: "mine", name: "Q", address: "wss://q.example.com/ws", kind: "gwar" });
    expect(s().bookmarks.map((b) => [b.id, b.password])).toEqual([["hidden", "theirs"], ["mine", undefined]]);
    expect(s().removedBookmarks.hidden).toBeUndefined();
  } finally {
    vi.useRealTimers();
  }
});

it("two devices that keep different twins of one server still end with one bookmark", async () => {
  const { mergeServers, fromBookmark } = await import("../connect/servers");
  const { dedupeBookmarks } = await import("./settings");
  const a = { id: "a", name: "Short", address: "gwar.app", kind: "gwar" as const, account: "acct", updatedAt: 10 };
  const b = { id: "b", name: "Long", address: "wss://gwar.app/ws", kind: "gwar" as const, account: "acct", updatedAt: 20 };
  vi.useFakeTimers({ now: 1_000 });
  try {
    // Each device prefers the one clicked; the second decides later.
    const first = dedupeBookmarks([b, a], {}, "acct", "a");
    vi.setSystemTime(2_000);
    const second = dedupeBookmarks([a, b], {}, "acct", "b");
    // Device one synced first, then device two merges its own decision in.
    const vault = mergeServers([], { bookmarks: first.bookmarks, removed: first.removed }, 2_000);
    const merged = mergeServers(vault, { bookmarks: second.bookmarks, removed: second.removed }, 2_000);
    expect(merged.filter((e) => !e.deleted).map((e) => e.id)).toEqual(["b"]);
    expect(fromBookmark(b).id).toBe("b");
  } finally {
    vi.useRealTimers();
  }
});

it.each(["text", "smooth", "game", "saver", "auto"] as const)("migrates saved %s quality on hydration and keeps independent fields across reloads", async screenPreset => {
  const data = new Map<string, string>([["gwar.settings", JSON.stringify({ version: 1, state: { screenPreset, theme: "dark" } })]]);
  const storage = { getItem: (key: string) => data.get(key) ?? null, setItem: (key: string, value: string) => data.set(key, value), removeItem: (key: string) => data.delete(key) };
  vi.stubGlobal("window", { localStorage: storage });
  vi.resetModules();
  const { useSettings } = await import("./settings");
  const { legacySpec, defaultQuality } = await import("../voice/screen-quality");
  const expected = screenPreset === "auto" ? defaultQuality : { screenMode: legacySpec(screenPreset).mode, screenHeight: legacySpec(screenPreset).height, screenFps: legacySpec(screenPreset).fps };
  expect(useSettings.getState()).toMatchObject(expected);
  const saved = JSON.parse(data.get("gwar.settings")!);
  expect(saved.version).toBe(SETTINGS_VERSION);
  expect(saved.state.screenPreset).toBeUndefined();
  useSettings.getState().setScreenQuality({ screenHeight: 1440 });
  useSettings.getState().setScreenQuality({ screenFps: 60 });
  await useSettings.persist.rehydrate();
  expect(useSettings.getState()).toMatchObject({ ...expected, screenHeight: 1440, screenFps: 60 });
});


it.each([6, 7].flatMap(version => ["KeyV", "AltRight", "CapsLock", "F24"].map(pttKey => ({ version, pttKey }))))("migrates v$version PTT $pttKey without losing existing settings", async ({ version, pttKey }) => {
  const previous = { language: "pl", theme: "dark", audio: { inputMode: "ptt", pttKey, inputDeviceId: "chosen-mic", noiseStrength: 70, vadThreshold: -64 }, desktop: { muteShortcut: "Control+KeyM", closeToTray: false }, sounds: { enabled: false, volume: 0.4 }, bookmarks: [{ id: "saved", name: "Local", address: "127.0.0.1", kind: "gwar", password: "keep-me" }], selfMuted: true, userVolumes: { friend: 0.7 } };
  const data = new Map([["gwar.settings", JSON.stringify({ version, state: previous })]]);
  vi.stubGlobal("window", { localStorage: { getItem: (key: string) => data.get(key) ?? null, setItem: (key: string, value: string) => data.set(key, value), removeItem: (key: string) => data.delete(key) } });
  vi.resetModules();
  const { useSettings } = await import("./settings");
  expect(useSettings.getState()).toMatchObject({ ...previous, audio: { ...previous.audio, pttReleaseDelay: 300 } });
  expect(JSON.parse(data.get("gwar.settings")!).version).toBe(8);
  useSettings.getState().setAudio({ pttKey: "Mouse5", pttReleaseDelay: 80 });
  await useSettings.persist.rehydrate();
  expect(useSettings.getState().audio).toMatchObject({ pttKey: "Mouse5", pttReleaseDelay: 80, inputDeviceId: "chosen-mic" });
  useSettings.getState().setAudio({ pttReleaseDelay: NaN });
  expect(useSettings.getState().audio.pttReleaseDelay).toBe(300);
});
