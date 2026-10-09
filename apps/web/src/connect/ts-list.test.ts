import { describe, expect, it } from "vitest";
import { addEntry, cleanName, deleteEntry, emptyList, normalizeList, renameEntry, resolveIdentity, sameList, setDefault, shortUid, type TsEntry, type TsList } from "./ts-list";

const e = (uid: string, name = uid): TsEntry => ({ uid, name, identity: `1V${uid}` });
const list = (uids: string[], def: string | null = uids[0] ?? null): TsList => ({ default: def, identities: uids.map((u) => e(u)) });
const uids = (l: TsList) => l.identities.map((x) => x.uid);

describe("addEntry", () => {
  it("the first identity becomes the default, later ones don't", () => {
    const first = addEntry(emptyList(), e("a"));
    expect(first.added).toBe(true);
    expect(first.list).toEqual({ default: "a", identities: [e("a")] });
    const second = addEntry(first.list, e("b"));
    expect(second.list.default).toBe("a");
    expect(uids(second.list)).toEqual(["a", "b"]);
  });

  it("dedupes by uid and leaves the existing entry alone", () => {
    const l = list(["a", "b"], "b");
    const again = addEntry(l, { uid: "a", name: "Other name", identity: "9Vdifferent" });
    expect(again.added).toBe(false);
    expect(again.list).toBe(l);
  });

  it("trims the name and falls back to Default; extra fields don't leak in", () => {
    const withExtras = { ...e("a", "  Main  "), level: 8, source: "TeamSpeak 3" } as TsEntry;
    expect(addEntry(emptyList(), withExtras).list.identities).toEqual([{ uid: "a", name: "Main", identity: "1Va" }]);
    expect(addEntry(emptyList(), e("b", "   ")).list.identities[0]?.name).toBe("Default");
  });
});

describe("renameEntry", () => {
  it("renames one entry, trims, cuts long names and ignores an empty name", () => {
    const l = list(["a", "b"]);
    expect(renameEntry(l, "b", "  Work  ").identities.map((x) => x.name)).toEqual(["a", "Work"]);
    expect(renameEntry(l, "b", "   ").identities[1]?.name).toBe("b");
    expect([...renameEntry(l, "a", "x".repeat(100)).identities[0]!.name]).toHaveLength(48);
    expect(renameEntry(l, "zzz", "Nope")).toEqual(l);
    expect(cleanName("  hi  ", "fb")).toBe("hi");
  });
});

describe("deleteEntry", () => {
  it("deleting the default makes the first remaining one the default", () => {
    expect(deleteEntry(list(["a", "b", "c"], "a"), "a")).toEqual(list(["b", "c"], "b"));
    expect(deleteEntry(list(["a", "b", "c"], "c"), "c")).toEqual(list(["a", "b"], "a"));
  });

  it("deleting another one keeps the default; deleting the last leaves an empty list", () => {
    expect(deleteEntry(list(["a", "b", "c"], "b"), "c")).toEqual(list(["a", "b"], "b"));
    expect(deleteEntry(list(["a"]), "a")).toEqual(emptyList());
    expect(deleteEntry(list(["a"]), "unknown")).toEqual(list(["a"]));
  });
});

describe("setDefault", () => {
  it("only accepts uids in the list", () => {
    const l = list(["a", "b"]);
    expect(setDefault(l, "b").default).toBe("b");
    expect(setDefault(l, "zzz")).toBe(l);
  });
});

describe("normalizeList", () => {
  it("drops repeats and entries without a uid or key, and repairs the default", () => {
    const messy: TsList = {
      default: "gone",
      identities: [e("a"), { ...e("a"), name: "dup" }, { uid: "", name: "x", identity: "1Vx" }, { uid: "c", name: "x", identity: "" }, e("b")],
    };
    expect(normalizeList(messy)).toEqual(list(["a", "b"], "a"));
    expect(normalizeList({ default: "b", identities: [e("a"), e("b")] }).default).toBe("b");
    expect(normalizeList({ default: "a", identities: [] })).toEqual(emptyList());
  });
});

describe("resolveIdentity (bookmarks)", () => {
  it("uses the remembered uid while it exists and falls back to the default when it doesn't", () => {
    const l = list(["a", "b", "c"], "b");
    expect(resolveIdentity(l, "c")).toBe("c");
    expect(resolveIdentity(l, "deleted-since")).toBe("b");
    expect(resolveIdentity(l, undefined)).toBe("b");
    expect(resolveIdentity(l, null)).toBe("b");
    expect(resolveIdentity(emptyList(), "a")).toBeNull();
    // A default that no longer exists falls to the first.
    expect(resolveIdentity({ default: "gone", identities: [e("a")] }, "x")).toBe("a");
  });
});

describe("helpers", () => {
  it("compares lists by content and order", () => {
    expect(sameList(list(["a", "b"]), list(["a", "b"]))).toBe(true);
    expect(sameList(list(["a", "b"]), list(["b", "a"]))).toBe(false);
    expect(sameList(list(["a", "b"], "a"), list(["a", "b"], "b"))).toBe(false);
    expect(sameList(list(["a"]), { default: "a", identities: [{ ...e("a"), name: "renamed" }] })).toBe(false);
  });

  it("shortens a unique id", () => {
    expect(shortUid("abcdefghijklmnopqrstuvwxyz=")).toBe("abcdefghij…");
    expect(shortUid("short")).toBe("short");
  });
});
