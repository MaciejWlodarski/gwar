/**
 * Tiny typed i18n: `en` defines the keys, `pl` must provide all of them
 * (checked by the compiler). Placeholders look like `{name}`.
 */
import { useMemo } from "react";
import { useSettings, type Language } from "../state/settings";
import { countKey, translate, type Key, type Params } from "./translate";

export { countKey, translate };
export type { Key, Params };

/** Translate outside React using the current language. */
export function tNow(key: Key, params?: Params): string {
  return translate(useSettings.getState().language, key, params);
}

export type TFn = (key: Key, params?: Params) => string;

export function useT(): TFn {
  const language = useSettings((s) => s.language);
  return useMemo(() => (key, params) => translate(language, key, params), [language]);
}

export function useLanguage(): Language {
  return useSettings((s) => s.language);
}
