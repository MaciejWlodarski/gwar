/** At start: whether any data this app keeps was saved by a newer version (docs/data-migrations.md). */
import { KV_DATABASE, KV_MIGRATIONS } from "../net/kv";
import { MESSAGE_STORAGE_DATABASE, MESSAGE_STORAGE_MIGRATIONS } from "../net/message-storage";
import { mlsDatabase, MLS_MIGRATIONS } from "../mls/storage";
import { localStorageError } from "./local-storage";
import { NewerDataError } from "./stored-data";

/**
 * The first store of a newer version found, or why localStorage cannot be used; undefined if
 * none. Databases the browser cannot list are checked anyway when they are opened.
 */
export async function storedDataProblem(factory: IDBFactory | undefined = globalThis.indexedDB): Promise<unknown> {
  const local = localStorageError();
  if (local !== undefined) return local;
  const known = new Map([[KV_DATABASE, KV_MIGRATIONS.length], [MESSAGE_STORAGE_DATABASE, MESSAGE_STORAGE_MIGRATIONS.length]]);
  const databases = await factory?.databases?.().catch(() => undefined) ?? [];
  for (const { name, version } of databases) {
    if (name === undefined || version === undefined) continue;
    const newest = name.startsWith(mlsDatabase("")) ? MLS_MIGRATIONS.length : known.get(name);
    if (newest !== undefined && version > newest) return new NewerDataError(name, version, newest);
  }
  return undefined;
}
