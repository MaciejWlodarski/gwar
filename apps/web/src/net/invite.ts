import { displayText } from "../lib/text";
import { parseServerAddress } from "./address";
import { decodeFixed, normalizeEndpoint } from "./handshake";
/**
 * Invite links carry `server=<canonical endpoint>`, `invite=CODE` and `server_key=<public key>`, and
 * `owner=<uid>` once the server has an owner (checked on first contact, docs/e2e-build.md section 6).
 * (the web app names the server to join). Without `server` the web app's own
 * origin is the server, which is only the case when one host serves both.
 */

const hasControl = (text: string): boolean => [...text].some(c => c.charCodeAt(0) < 32 || c.charCodeAt(0) === 127);

const CODE_RE = /^[A-Za-z0-9_-]{3,128}$/;

export interface InviteTarget {
  /** Display hint supplied by the inviter, verified only after joining. */
  name?: string;
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
  if (["invite", "server", "server_key", "owner"].some(key => params.getAll(key).length > 1)) return null;
  const code = params.get("invite") ?? "";
  if (!isValidInviteCode(code)) return null;
  const serverKey = params.get("server_key");
  try { decodeFixed(serverKey, 32); } catch { return null; }
  const owner = params.get("owner") ?? undefined;
  if (owner !== undefined) { try { decodeFixed(owner, 20); } catch { return null; } }
  const name = params.get("name") ?? undefined;
  if (params.getAll("name").length > 1 || (name !== undefined && (!name.trim() || name.length > 128 || displayText(name) !== name))) return null;
  const server = params.get("server") ?? undefined;
  if (server !== undefined && (!server || server.length > 2048 || displayText(server) !== server || (hasControl(server) || /[\s\\]/u.test(server)) || !parseServerAddress(server, { protocol: "tauri:", hostname: "", port: "" }, true).ok)) return null;
  const target: InviteTarget = server ? { code, server, serverKey: serverKey! } : { code, serverKey: serverKey! };
  return { ...target, ...(owner ? { owner } : {}), ...(name ? { name } : {}) };
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
  params.delete("name");
  const rest = params.toString();
  return { target, rest: rest ? `?${rest}` : "" };
}

/**
 * Recognises a pasted invite link. The server is the link's `server` parameter,
 * else the link's own host (`host[:port]`).
 */
export function parseInviteLink(input: string): InviteTarget | null {
  const text = input.trim();
  if (text.length > 4096 || (hasControl(text) || /[\s\\]/u.test(text)) || !/^https?:\/\//i.test(text)) return null;
  let url: URL;
  try {
    url = new URL(text);
  } catch {
    return null;
  }
  if (url.username || url.password || url.hash || url.pathname !== "/") return null;
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
  name?: string;
}

/** Include the exact endpoint and verified full public-key fingerprint. */
export function buildInviteLink({ webOrigin, serverEndpoint, serverKey, code, owner, name }: BuildInviteOptions): string {
  decodeFixed(serverKey, 32);
  if (owner) decodeFixed(owner, 20);
  const label = displayText(name ?? "").trim().slice(0, 128);
  const web = new URL(webOrigin);
  const params = new URLSearchParams({ server: normalizeEndpoint(serverEndpoint), invite: code, server_key: serverKey, ...(owner ? { owner } : {}), ...(label ? { name: label } : {}) });
  return `${web.origin}/?${params.toString()}`;
}

/** The only accepted desktop route. No arbitrary navigation, commands, files, or implicit origin. */
export function parseDesktopInvite(input: string): InviteTarget | null {
  if (!/^gwar:\/\/invite\/?\?/.test(input) || input.length > 4096 || (hasControl(input) || /[\s\\]/u.test(input))) return null;
  let url: URL;
  try { url = new URL(input); } catch { return null; }
  if (url.protocol !== "gwar:" || url.hostname !== "invite" || url.port || url.username || url.password || (url.pathname !== "" && url.pathname !== "/") || url.hash) return null;
  if ([...url.searchParams.keys()].some(k => !["invite", "server", "server_key", "owner", "name"].includes(k))) return null;
  const target = fromParams(url.searchParams);
  return target?.server ? target : null;
}

export function desktopInviteLink(target: InviteTarget): string | null {
  if (!target.server) return null;
  const params = new URLSearchParams({ server: target.server, invite: target.code, server_key: target.serverKey, ...(target.owner ? { owner: target.owner } : {}), ...(target.name ? { name: target.name } : {}) });
  const link = `gwar://invite/?${params}`;
  return parseDesktopInvite(link) ? link : null;
}
