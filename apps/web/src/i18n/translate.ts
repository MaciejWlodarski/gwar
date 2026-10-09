/** Pure translation helpers (no store access, so they are unit-testable). */
import type { Language } from "../state/settings";
import { en, type Key } from "./en";
import { pl } from "./pl";

export type { Key };
export type Params = Record<string, string | number>;

export const dictionaries: Record<Language, Record<Key, string>> = { en, pl };

export function translate(language: Language, key: Key, params?: Params): string {
  const template = dictionaries[language][key] ?? en[key];
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (_, name: string) => String(params[name] ?? `{${name}}`));
}

const pluralRules: Record<Language, Intl.PluralRules> = { en: new Intl.PluralRules("en"), pl: new Intl.PluralRules("pl") };

/** Picks `<base>.one|few|many|other` for a count (falls back to `.other`). */
export function countKey(language: Language, base: "chat.users", count: number): Key {
  const category = pluralRules[language].select(count);
  const candidate = `${base}.${category}` as Key;
  return candidate in dictionaries[language] ? candidate : (`${base}.other` as Key);
}
