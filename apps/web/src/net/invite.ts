/**
 * Invite links: `https://app.example.org/?server=voice.example.com&invite=CODE`
 * (the web app names the server to join). Without `server` the web app's own
 * origin is the server, which is only the case when one host serves both.
 */

const CODE_RE = /^[A-Za-z0-9_-]{3,128}$/;

export interface InviteTarget {
  code: string;
  /** Address of the server as a person would type it; absent means "the server on the web app's own origin". */
  server?: string;
}

export function isValidInviteCode(code: string): boolean {
  return CODE_RE.test(code);
}

function fromParams(params: URLSearchParams): InviteTarget | null {
  const code = params.get("invite")?.trim() ?? "";
  if (!isValidInviteCode(code)) return null;
  const server = params.get("server")?.trim();
  return server ? { code, server } : { code };
}

/** Reads `?invite=` / `&server=` from a page's search string; returns the cleaned search string too. */
export function readInviteFromSearch(search: string): { target: InviteTarget | null; rest: string } {
  const params = new URLSearchParams(search);
  const target = fromParams(params);
  if (!params.has("invite") && !params.has("server")) return { target: null, rest: search };
  params.delete("invite");
  params.delete("server");
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
  return { code: target.code, server: target.server ?? url.host };
}

export interface BuildInviteOptions {
  /** Public address of the web app (see `webOrigin`): the official one in the desktop app. */
  webOrigin: string;
  /** HTTP origin of the connected server, e.g. `https://voice.example.com`. */
  serverOrigin: string;
  code: string;
}

/**
 * The shareable link for a code: `<web app>/?server=<host[:port]>&invite=<code>`.
 * `server` is left out only when the web app and the server share an origin; a
 * server on another scheme than the web app is written as a full origin so the
 * scheme survives.
 */
export function buildInviteLink({ webOrigin, serverOrigin, code }: BuildInviteOptions): string {
  const web = new URL(webOrigin);
  const server = new URL(serverOrigin);
  const params = new URLSearchParams();
  if (web.host.toLowerCase() !== server.host.toLowerCase() || web.protocol !== server.protocol) {
    params.set("server", web.protocol === server.protocol ? server.host : server.origin);
  }
  params.set("invite", code);
  return `${web.origin}/?${params.toString()}`;
}
