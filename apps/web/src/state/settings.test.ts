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
