import { describe, expect, it } from "vitest";
import { en } from "./en";
import { pl } from "./pl";
import { countKey, translate } from "./translate";

const placeholders = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();

describe("dictionaries", () => {
  it("polish defines exactly the english keys", () => {
    expect(Object.keys(pl).sort()).toEqual(Object.keys(en).sort());
  });

  it("placeholders match between languages", () => {
    for (const key of Object.keys(en) as Array<keyof typeof en>) {
      expect(placeholders(pl[key]), key).toEqual(placeholders(en[key]));
    }
  });

  it("interpolates and leaves unknown placeholders visible", () => {
    expect(translate("en", "sys.user_joined_channel", { name: "Ann" })).toBe("Ann joined the channel");
    expect(translate("pl", "sys.user_joined_channel")).toBe("{name} dołącza do kanału");
  });

  it("uses polish plural categories", () => {
    expect(translate("pl", countKey("pl", "chat.users", 1), { count: 1 })).toBe("1 osoba na głosie");
    expect(translate("pl", countKey("pl", "chat.users", 3), { count: 3 })).toBe("3 osoby na głosie");
    expect(translate("pl", countKey("pl", "chat.users", 5), { count: 5 })).toBe("5 osób na głosie");
    expect(translate("en", countKey("en", "chat.users", 1), { count: 1 })).toBe("1 user in voice");
    expect(translate("en", countKey("en", "chat.users", 2), { count: 2 })).toBe("2 users in voice");
  });
});
