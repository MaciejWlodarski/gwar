/**
 * The list of TeamSpeak identities (docs/connect.md, "Vault"): the same shape on the
 * desktop, in the vault and in the UI. Plain functions that return a new list, so the
 * rules (one entry per uid, a default that always exists) are tested without a desktop.
 */

/** One identity: its TeamSpeak unique id, a name the person chose, and `<counter>V<obfuscated key>`. */
export interface TsEntry {
  uid: string;
  name: string;
  identity: string;
}

export interface TsList {
  /** The uid used when nothing else is chosen; one of `identities`, or null when there are none. */
  default: string | null;
  identities: TsEntry[];
}

export const emptyList = (): TsList => ({ default: null, identities: [] });

/** The name of the identity a device starts with. */
export const DEFAULT_NAME = "Default";
/** The name given to the single identity of the older vault format. */
export const LEGACY_NAME = "TeamSpeak";
export const MAX_NAME_LENGTH = 48;

/** Names are trimmed and cut; an empty one becomes `fallback`. */
export function cleanName(name: string, fallback: string): string {
  return [...name.trim()].slice(0, MAX_NAME_LENGTH).join("").trim() || fallback;
}

/** `abcdefghij…`, for showing a unique id in a row; the full one is what gets copied. */
export const shortUid = (uid: string): string => (uid.length > 12 ? `${uid.slice(0, 10)}…` : uid);

/** Drops entries without a uid or key and repeated uids (the first stays); repairs a default that is not in the list. */
export function normalizeList(list: TsList): TsList {
  const seen = new Set<string>();
  const identities = list.identities.filter((e) => {
    if (!e.uid || !e.identity || seen.has(e.uid)) return false;
    seen.add(e.uid);
    return true;
  });
  const keep = list.default !== null && seen.has(list.default);
  return { default: keep ? list.default : (identities[0]?.uid ?? null), identities };
}

/** Adds an entry. If its uid is already there nothing changes (`added` is false). The first entry becomes the default. */
export function addEntry(list: TsList, entry: TsEntry): { list: TsList; added: boolean } {
  if (list.identities.some((e) => e.uid === entry.uid)) return { list, added: false };
  const next = { uid: entry.uid, name: cleanName(entry.name, DEFAULT_NAME), identity: entry.identity };
  return { list: normalizeList({ default: list.default, identities: [...list.identities, next] }), added: true };
}

export function renameEntry(list: TsList, uid: string, name: string): TsList {
  return { ...list, identities: list.identities.map((e) => (e.uid === uid ? { ...e, name: cleanName(name, e.name) } : e)) };
}

/** Removes an entry. Deleting the default makes the first remaining one the default. */
export function deleteEntry(list: TsList, uid: string): TsList {
  return normalizeList({ default: list.default, identities: list.identities.filter((e) => e.uid !== uid) });
}

/** Makes `uid` the default; unknown uids change nothing. */
export function setDefault(list: TsList, uid: string): TsList {
  return list.identities.some((e) => e.uid === uid) ? { ...list, default: uid } : list;
}

/**
 * The uid to connect with: the wanted one (a bookmark's, say) if it is still in the list,
 * otherwise the default. Null for an empty list.
 */
export function resolveIdentity(list: TsList, wanted?: string | null): string | null {
  return (wanted && list.identities.find((e) => e.uid === wanted)?.uid) || normalizeList(list).default;
}

/** True if both lists hold the same entries in the same order with the same default. */
export function sameList(a: TsList, b: TsList): boolean {
  return (
    a.default === b.default &&
    a.identities.length === b.identities.length &&
    a.identities.every((e, i) => {
      const other = b.identities[i];
      return other?.uid === e.uid && other.name === e.name && other.identity === e.identity;
    })
  );
}
