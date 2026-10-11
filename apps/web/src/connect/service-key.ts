import { CONNECT_URL } from "../lib/official";
import { decodeBase64Url, encodeBase64Url } from "../net/base64url";

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
  let url: URL;
  try { url = new URL(CONNECT_URL); } catch { return null; }
  // Plain HTTP is only for local development and the smoke test.
  const local = import.meta.env.DEV && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if (url.protocol !== "https:" && !(url.protocol === "http:" && local)) return null;
  let cached: string | null = null;
  try { cached = validKey(localStorage.getItem(storageKey)); } catch { /* storage may be disabled */ }
  try {
    const reply = await fetch(`${CONNECT_URL}/v1/service-key`, {
      redirect: "error", credentials: "omit", cache: "no-store", signal: AbortSignal.timeout(10_000),
    });
    if (reply.status !== 200) return cached;
    const key = validKey((await reply.json() as { key?: unknown }).key);
    if (!key) return cached;
    try { localStorage.setItem(storageKey, key); } catch { /* memory caching still works */ }
    return key;
  } catch {
    return cached;
  }
}
