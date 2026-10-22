import { connectJson, connectUrl } from "./transport";
import { CONNECT_URL } from "../lib/official";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";
import { appStorage } from "../lib/local-storage";

let request: Promise<string | null> | undefined;
const storageKey = `gwar.connect.service-key:${CONNECT_URL}`;

function validKey(value: unknown): string | null {
  if (typeof value !== "string" || !/^[A-Za-z0-9_-]{43}$/.test(value)) return null;
  try {
    const bytes = decodeBase64Url(value);
    return bytes.length === 32 && encodeBase64Url(bytes) === value ? value : null;
  } catch {
    return null;
  }
}

/** Once per app session, only from the client's configured Connect service. */
export function loadServiceKey(): Promise<string | null> {
  return request ??= fetchKey();
}

async function fetchKey(): Promise<string | null> {
  try { connectUrl(CONNECT_URL, "/v1/service-key"); } catch { return null; }
  let cached: string | null = null;
  try { cached = validKey(appStorage.getItem(storageKey)); } catch { /* storage may be disabled */ }
  try {
    const { response, data } = await connectJson(CONNECT_URL, "/v1/service-key");
    if (response.status !== 200) return cached;
    const key = validKey((data as { key?: unknown } | null)?.key);
    if (!key) return cached;
    try { appStorage.setItem(storageKey, key); } catch { /* memory caching still works */ }
    return key;
  } catch {
    return cached;
  }
}
