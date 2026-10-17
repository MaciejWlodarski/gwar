import { decodeFixed, normalizeEndpoint } from "./handshake";
/**
 * Invite links carry `server=<canonical endpoint>`, `invite=CODE` and `server_key=<public key>`, and
 * `owner=<uid>` once the server has an owner (checked on first contact, docs/e2e-build.md section 6).
 * (the web app names the server to join). Without `server` the web app's own
 * origin is the server, which is only the case when one host serves both.
 */

const CODE_RE = /^[A-Za-z0-9_-]{3,128}$/;

export interface InviteTarget {
  code: string;
  serverKey: string;
  /** Address of the server as a person would type it; absent means "the server on the web app's own origin". */
  server?: string;
  /** The server's owner, as the inviter's device verified it. */
  owner?: string;
}

export function isValidInviteCode(code: string): boolean {
  return CODE_RE.test(code);
}

function fromParams(params: URLSearchParams): InviteTarget | null {
  const code = params.get("invite")?.trim() ?? "";
  if (!isValidInviteCode(code)) return null;
  const serverKey = params.get("server_key");
  try { decodeFixed(serverKey, 32); } catch { return null; }
  const owner = params.get("owner") ?? undefined;
  if (owner !== undefined) { try { decodeFixed(owner, 20); } catch { return null; } }
  const server = params.get("server")?.trim();
  const target: InviteTarget = server ? { code, server, serverKey: serverKey! } : { code, serverKey: serverKey! };
  return owner ? { ...target, owner } : target;
}

/** Reads `?invite=` / `&server=` from a page's search string; returns the cleaned search string too. */
export function readInviteFromSearch(search: string): { target: InviteTarget | null; rest: string } {
  const params = new URLSearchParams(search);
  const target = fromParams(params);
  if (!params.has("invite") && !params.has("server")) return { target: null, rest: search };
  params.delete("invite");
  params.delete("server");
  params.delete("server_key");
  params.delete("owner");
  const rest = params.toString();
  return { target, rest: rest ? `?${rest}` : "" };
}

/**
 * Recognises a pasted invite link. The server is the link's `server` parameter,
 * else the link's own host (`host[:port]`).
 */
export function parseInviteLink(input: string): InviteTarget | null {
  const text = input.trim();
  if (!/^https?:\/\//i.test(text)) return null;
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return null;
  }
  const target = fromParams(url.searchParams);
  if (!target) return null;
  return { ...target, server: target.server ?? `${url.protocol}//${url.host}` };
}

export interface BuildInviteOptions {
  webOrigin: string;
  serverEndpoint: string;
  serverKey: string;
  code: string;
  owner?: string;
}

/** Include the exact endpoint and verified full public-key fingerprint. */
export function buildInviteLink({ webOrigin, serverEndpoint, serverKey, code, owner }: BuildInviteOptions): string {
  decodeFixed(serverKey, 32);
  if (owner) decodeFixed(owner, 20);
  const web = new URL(webOrigin);
  const params = new URLSearchParams({ server: normalizeEndpoint(serverEndpoint), invite: code, server_key: serverKey, ...(owner ? { owner } : {}) });
  return `${web.origin}/?${params.toString()}`;
}
