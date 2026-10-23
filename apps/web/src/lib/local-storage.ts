/**
 * Everything the app keeps in localStorage (docs/data-migrations.md): `gwar.settings`,
 * `gwar.server-pin:*`, `gwar.channels:*`, `gwar.connect.handle-pin:*` and the Connect service key.
 *
 * localStorage has no transactions, and across tabs it is not even consistent: each tab's process
 * keeps its own copy and hears of other tabs' writes later. So nothing here decides an upgrade from
 * what localStorage shows. Each version keeps its entries in a namespace of its own (`gwar.vN.<id>.…`;
 * version 1 used the plain names), and IndexedDB, which is transactional, records which namespace is
 * the current one (the `local-storage` record of the `gwar` database). An upgrade runs in one tab at
 * a time, under a Web Lock: it writes a fresh namespace nobody reads yet, marks it complete, and only
 * then publishes it in IndexedDB, if the record still names the namespace it started from. A tab
 * that dies halfway leaves an unpublished namespace that the next attempt replaces with a new one,
 * so its late writes land nowhere. Tabs of the older version keep writing their own namespace, which
 * the new version never reads. `gwar.storage-version` mirrors the published namespace for the
 * synchronous checks and for other tabs (the `storage` event); it is written only after publishing.
 */
import { NewerDataError } from "./stored-data";
import { kvGet, kvUpdate } from "../net/kv";
import { LOCAL_STORAGE_MIGRATIONS } from "./local-storage-migrations";

export const LOCAL_STORAGE_VERSION_KEY = "gwar.storage-version";
export const LOCAL_STORAGE_VERSION = LOCAL_STORAGE_MIGRATIONS.length + 1;
/** The `gwar` database record naming the current namespace. */
export const LOCAL_STORAGE_AUTHORITY = "local-storage";
const LOCK = "gwar.local-storage";
/** Written last into a namespace by its upgrade: entries written before it are visible once it is. */
const COMPLETE = "gwar.storage-complete";
/** The app's keys, by their names at version 1: exact names, and prefixes ending in `:`. */
const APP_KEYS = ["gwar.settings", "gwar.server-pin:", "gwar.channels:", "gwar.connect.handle-pin:", "gwar.connect.service-key:"];
const VERSIONED = /^gwar\.v([0-9]+)\.([0-9a-f]{16})\.(.*)$/s;
const STORE = "This browser's Gwar settings";

/** A version's place in localStorage: version 1 the plain names (namespace ""), a later one its own. */
export interface LocalStorageLayout { version: number; namespace: string }
export type LocalStorageMigration = (storage: Storage, from: LocalStorageLayout, to: LocalStorageLayout) => void;
type Authority = LocalStorageLayout & { previous?: LocalStorageLayout };

const FIRST: LocalStorageLayout = { version: 1, namespace: "" };
const same = (a: LocalStorageLayout | undefined, b: LocalStorageLayout | undefined) => !!a && !!b && a.version === b.version && a.namespace === b.namespace;

function validLayout(value: unknown): value is LocalStorageLayout {
  if (typeof value !== "object" || value === null) return false;
  const { version, namespace } = value as Partial<LocalStorageLayout>;
  if (typeof version !== "number" || !Number.isSafeInteger(version) || version < 1) return false;
  return version === 1 ? namespace === "" : typeof namespace === "string" && /^[0-9a-f]{16}$/.test(namespace);
}

/** `gwar.storage-version` as a layout; undefined if it is not one. Missing means version 1. */
export function parseLocalStorageLayout(raw: string | null): LocalStorageLayout | undefined {
  if (raw === null || raw === "1") return FIRST;
  const m = /^([1-9][0-9]{0,8})\.([0-9a-f]{16})$/.exec(raw);
  const layout = m ? { version: Number(m[1]), namespace: m[2]! } : undefined;
  return validLayout(layout) ? layout : undefined;
}
export const formatLocalStorageLayout = (layout: LocalStorageLayout) => layout.version === 1 ? "1" : `${layout.version}.${layout.namespace}`;

const isAppKey = (key: string) => APP_KEYS.some((p) => (p.endsWith(":") ? key.startsWith(p) : key === p));

/**
 * The name `key` (one of the app's, by its version-1 name) has in `layout`; by default the current
 * one. Throws while localStorage is not usable.
 */
export function localStorageKey(key: string, layout: LocalStorageLayout | undefined = currentLayout()): string {
  if (!layout) throw localStorageProblem();
  if (!isAppKey(key) && key !== COMPLETE) throw new Error(`not an app localStorage key: ${key}`);
  return layout.version === 1 ? key : `gwar.v${layout.version}.${layout.namespace}.${key.slice("gwar.".length)}`;
}

/** The layout a stored name belongs to, and its version-1 name; undefined for other keys. */
function placeOf(stored: string): { layout: LocalStorageLayout; key: string } | undefined {
  if (isAppKey(stored) || stored === COMPLETE) return { layout: FIRST, key: stored };
  const m = VERSIONED.exec(stored);
  return m ? { layout: { version: Number(m[1]), namespace: m[2]! }, key: `gwar.${m[3]}` } : undefined;
}

function storedKeys(storage: Storage): string[] {
  const keys: string[] = [];
  for (let i = 0; i < storage.length; i++) {
    const key = storage.key(i);
    if (key !== null) keys.push(key);
  }
  return keys;
}

/** Copies every entry of `from` into `to`, through `change` (null leaves it out). `from` stays as it is. */
export function copyLocalStorageForward(storage: Storage, from: LocalStorageLayout, to: LocalStorageLayout, change: (key: string, value: string) => string | null = (_, value) => value): void {
  for (const stored of storedKeys(storage)) {
    const place = placeOf(stored);
    if (!place || place.key === COMPLETE || !same(place.layout, from)) continue;
    const value = storage.getItem(stored);
    const next = value === null ? null : change(place.key, value);
    if (next !== null) storage.setItem(localStorageKey(place.key, to), next);
  }
}

/** Removes every namespace of the app's but `keep`: older versions and abandoned upgrades. */
function dropNamespaces(storage: Storage, keep: (LocalStorageLayout | undefined)[]): void {
  for (const stored of storedKeys(storage)) {
    const place = placeOf(stored);
    if (place && !keep.some((layout) => same(layout, place.layout))) storage.removeItem(stored);
  }
}

let problem: unknown;
let active: LocalStorageLayout | undefined;
const listeners = new Set<() => void>();
/** Tests only: this page's view of localStorage, as one tab of several sees it. */
let view: Storage | undefined;
const browserStorage = (): Storage => view ?? localStorage;

function storageOrUndefined(): Storage | undefined {
  try {
    const storage = view ?? (typeof localStorage === "undefined" ? undefined : localStorage);
    storage?.getItem(LOCAL_STORAGE_VERSION_KEY);
    return storage;
  } catch {
    return undefined;
  }
}

function fail(error: unknown): void {
  if (problem !== undefined) return;
  problem = error;
  for (const listener of [...listeners]) listener();
}

function localStorageProblem(): unknown {
  return problem ?? new Error("Gwar's saved settings are being upgraded by another tab; reload the page.");
}

/**
 * The layout this page reads and writes, or undefined: localStorage holds a newer version (then for
 * good), or this version's namespace is not published, or not yet visible in this tab.
 */
function currentLayout(): LocalStorageLayout | undefined {
  if (problem !== undefined) return undefined;
  const storage = storageOrUndefined();
  if (!storage) return active ?? FIRST;
  const hinted = parseLocalStorageLayout(storage.getItem(LOCAL_STORAGE_VERSION_KEY));
  if (!hinted || hinted.version > LOCAL_STORAGE_VERSION) {
    fail(new NewerDataError(STORE, hinted?.version, LOCAL_STORAGE_VERSION));
    return undefined;
  }
  if (active) return active;
  // Published (the mirror is written only after that) and complete in this tab's copy.
  if (hinted.version === LOCAL_STORAGE_VERSION && (hinted.version === 1 || storage.getItem(localStorageKey(COMPLETE, hinted)) !== null)) active = hinted;
  return active;
}

async function readAuthority(): Promise<Authority> {
  const stored = await kvGet<unknown>(LOCAL_STORAGE_AUTHORITY);
  if (stored === undefined) return FIRST;
  if (!validLayout(stored)) throw new NewerDataError(STORE, undefined, LOCAL_STORAGE_VERSION);
  const previous = (stored as Authority).previous;
  return { version: stored.version, namespace: stored.namespace, previous: validLayout(previous) ? previous : undefined };
}

/**
 * Until the entries of `layout` that another tab wrote are visible here: its marker came last, and a
 * tab's writes reach the others in order. A marker still missing after 10 s was removed with the
 * rest (localStorage cleared outside the app): what is left is the namespace, marked again.
 */
async function visible(storage: Storage, layout: LocalStorageLayout): Promise<void> {
  if (layout.version === 1) return;
  const marker = localStorageKey(COMPLETE, layout);
  for (let waited = 0; storage.getItem(marker) === null; waited += 50) {
    if (waited >= 10_000) {
      console.warn("Gwar's saved settings were removed outside the app; starting from what is left");
      storage.setItem(marker, "1");
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

function newNamespace(): string {
  return [...crypto.getRandomValues(new Uint8Array(8))].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Brings localStorage to this version before the app reads it: at start, before the first render.
 * Upgrades run under a lock shared by the tabs and are published in IndexedDB; data of a newer
 * version (there or in the mirror) makes localStorage unusable, untouched. Never throws: a problem
 * is what `localStorageError` reports.
 */
export async function prepareLocalStorage(migrations: readonly LocalStorageMigration[] = LOCAL_STORAGE_MIGRATIONS): Promise<void> {
  const current = migrations.length + 1;
  const storage = storageOrUndefined();
  if (!storage || problem !== undefined) return;
  try {
    const ready = currentLayout();
    if (problem !== undefined) return;
    // Without IndexedDB nothing can be upgraded, but data of this version still is usable.
    const published = await readAuthority().catch((error: unknown) => { if (ready && !(error instanceof NewerDataError)) return ready; throw error; });
    if (published.version > current) return fail(new NewerDataError(STORE, published.version, current));
    if (ready && same(ready, published)) return;
    const locks = typeof navigator === "undefined" ? undefined : navigator.locks;
    if (!locks) return fail(new Error("This browser cannot upgrade Gwar's saved settings safely (no Web Locks)."));
    await locks.request(LOCK, async () => {
      let from = await readAuthority();
      if (from.version > current) return fail(new NewerDataError(STORE, from.version, current));
      await visible(storage, from);
      // Abandoned upgrades and versions before the previous one go; the previous stays for its tabs.
      dropNamespaces(storage, [from.previous, from]);
      while (from.version < current) {
        const start = from;
        const to = { version: start.version + 1, namespace: newNamespace() };
        migrations[start.version - 1]!(storage, start, to);
        storage.setItem(localStorageKey(COMPLETE, to), "1");
        await kvUpdate<unknown>(LOCAL_STORAGE_AUTHORITY, (stored) => {
          if (!same(stored === undefined ? FIRST : (stored as LocalStorageLayout), start)) throw new Error("Gwar's saved settings were upgraded meanwhile; reload the page.");
          return { ...to, previous: { version: start.version, namespace: start.namespace } };
        });
        storage.setItem(LOCAL_STORAGE_VERSION_KEY, formatLocalStorageLayout(to));
        dropNamespaces(storage, [start, to]);
        from = { ...to, previous: { version: start.version, namespace: start.namespace } };
      }
      if (storage.getItem(LOCAL_STORAGE_VERSION_KEY) !== formatLocalStorageLayout(from) && from.version > 1) {
        storage.setItem(LOCAL_STORAGE_VERSION_KEY, formatLocalStorageLayout(from));
      }
      active = { version: from.version, namespace: from.namespace };
    });
  } catch (error) {
    fail(error);
  }
}

/**
 * Whether the app may read and write its localStorage now. Every call checks the mirror again: a
 * tab of a newer version may have upgraded it meanwhile, and then it stays unusable for this page.
 * Storage that is unavailable altogether counts as usable: each reader already copes with that.
 */
export function localStorageUsable(): boolean {
  return currentLayout() !== undefined;
}

/** Why localStorage is not used, if it is not. */
export function localStorageError(): unknown {
  currentLayout();
  return problem;
}

/**
 * Calls `listener` once localStorage becomes unusable: at once if a newer version's tab upgrades it
 * (the `storage` event), else at the next read or write.
 */
export function watchLocalStorage(listener: () => void): () => void {
  listeners.add(listener);
  const changed = (event: StorageEvent) => {
    if (event.key === LOCAL_STORAGE_VERSION_KEY || event.key === null) currentLayout();
  };
  globalThis.addEventListener?.("storage", changed);
  return () => { listeners.delete(listener); globalThis.removeEventListener?.("storage", changed); };
}

/**
 * For zustand's `persist`: this version's entries, and nothing read or written while localStorage
 * is not usable.
 */
export function guardedLocalStorage(storage: Storage | undefined) {
  const name = (key: string) => { const layout = currentLayout(); return layout && localStorageKey(key, layout); };
  return {
    getItem: (key: string) => { const n = name(key); return n === undefined ? null : storage?.getItem(n) ?? null; },
    setItem: (key: string, value: string) => { const n = name(key); if (n !== undefined) storage?.setItem(n, value); },
    removeItem: (key: string) => { const n = name(key); if (n !== undefined) storage?.removeItem(n); },
  };
}

/**
 * localStorage for the app's own keys (pins, channel records, caches), under this version's names:
 * it fails, as unavailable storage does, while localStorage is not usable. Every reader fails closed.
 */
export const appStorage: Pick<Storage, "getItem" | "setItem" | "removeItem"> = {
  getItem: (key) => browserStorage().getItem(localStorageKey(key)),
  setItem: (key, value) => browserStorage().setItem(localStorageKey(key), value),
  removeItem: (key) => browserStorage().removeItem(localStorageKey(key)),
};

/** The version-1 name of a stored key of this version's, if it is one (for `storage` events). */
export function appKeyOf(stored: string): string | undefined {
  const layout = currentLayout();
  const place = layout && placeOf(stored);
  return place && same(place.layout, layout) && place.key !== COMPLETE ? place.key : undefined;
}

/** Tests only: use `storage` as this page's localStorage. */
export function useLocalStorageForTests(storage: Storage): void {
  view = storage;
}
