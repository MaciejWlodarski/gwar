import { afterEach, describe, expect, it, vi } from "vitest";
import { defaultNickname, legacyNickname, validNickname } from "./nickname";
import { generateIdentity, importIdentity, loadOrCreateIdentity, type IdentityBackup, type IdentityStore } from "./identity";

afterEach(() => vi.unstubAllGlobals());

describe("identity nicknames", () => {
  it("generates five crypto-random digits, including leading zeroes, rejecting biased bytes", () => {
    let calls = 0;
    vi.stubGlobal("crypto", { getRandomValues: (bytes: Uint8Array) => { calls++; bytes.set([255, 250, 0, 1, 2, 3, 4, 5]); return bytes; } });
    expect(defaultNickname()).toBe("gwar-01234");
    expect(calls).toBe(1);
  });
  it("validates trimmed Unicode characters against the protocol limit", () => {
    expect(validNickname("  Łucja  ")).toBe("Łucja");
    expect(validNickname("😀".repeat(32))).toBe("😀".repeat(32));
    expect(validNickname("😀".repeat(33))).toBeUndefined();
    expect(validNickname("   ")).toBeUndefined();
  });
  it("migrates last-used names, falling back to bookmarks", () => {
    vi.stubGlobal("localStorage", { getItem: () => JSON.stringify({ state: { lastNickname: "  Anna ", bookmarks: [{ nickname: "Bob" }] } }) });
    expect(legacyNickname()).toBe("Anna");
    vi.stubGlobal("localStorage", { getItem: () => JSON.stringify({ state: { bookmarks: [{ nickname: "Łukasz" }] } }) });
    expect(legacyNickname()).toBe("Łukasz");
  });
  it("migrates an existing key once without losing the identity and keeps the nickname in backups", async () => {
    const existing = await generateIdentity();
    let backup: IdentityBackup = existing.exportBackup();
    delete backup.nickname;
    vi.stubGlobal("localStorage", { getItem: () => JSON.stringify({ state: { lastNickname: "Previous" } }) });
    const store: IdentityStore = { load: async () => backup, save: async (b) => { backup = b; } };
    const restored = await loadOrCreateIdentity(store);
    expect(restored.publicKey).toBe(existing.publicKey);
    expect(restored.nickname).toBe("Previous");
    expect(backup.nickname).toBe("Previous");
    vi.stubGlobal("localStorage", { getItem: () => null });
    expect((await loadOrCreateIdentity(store)).nickname).toBe("Previous");
    expect((await importIdentity(JSON.stringify(backup), store)).nickname).toBe("Previous");
  });
  it("new identities get a persisted random default instead of reusing an old connection nickname", async () => {
    let saved: IdentityBackup | undefined;
    vi.stubGlobal("localStorage", { getItem: () => JSON.stringify({ state: { lastNickname: "Previous" } }) });
    const store: IdentityStore = { load: async () => saved, save: async (b) => { saved = b; } };
    const identity = await loadOrCreateIdentity(store);
    expect(identity.nickname).toMatch(/^gwar-\d{5}$/);
    expect((await loadOrCreateIdentity(store)).nickname).toBe(identity.nickname);
  });
});
