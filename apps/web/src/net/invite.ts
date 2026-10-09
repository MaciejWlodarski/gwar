/**
 * Invite links: `https://chat.example.com/?invite=CODE` (hosted web client: the
 * page's own origin is the server) or, when the page is not the server,
 * `...?invite=CODE&server=voice.example.com:8790`.
 */

const CODE_RE = /^[A-Za-z0-9_-]{3,128}$/;

export interface InviteTarget {
  code: string;
  /** Address of the server as a person would type it; absent means "the server that served this page". */
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
  /** `location.origin` of the page (`tauri://localhost` and the like in the desktop app). */
  pageOrigin: string;
  /** HTTP origin of the connected server, e.g. `https://voice.example.com`. */
  serverOrigin: string;
  code: string;
}

/**
 * The shareable link for a code. A web page that is not the server itself names
 * the server in the link; in the desktop app the link points at the server's own
 * origin (which serves the web client when it hosts one).
 */
export function buildInviteLink({ pageOrigin, serverOrigin, code }: BuildInviteOptions): string {
  const params = new URLSearchParams({ invite: code });
  let base = serverOrigin;
  if (/^https?:\/\//i.test(pageOrigin)) {
    base = new URL(pageOrigin).origin;
    const server = new URL(serverOrigin);
    const page = new URL(pageOrigin);
    if (page.host.toLowerCase() !== server.host.toLowerCase() || page.protocol !== server.protocol) {
      params.set("server", page.protocol === server.protocol ? server.host : server.origin);
    }
  }
  return `${base.replace(/\/+$/, "")}/?${params.toString()}`;
}
