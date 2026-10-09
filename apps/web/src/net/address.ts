/**
 * Turns whatever a person types into the WebSocket URL of a vc/1 server.
 *
 * Rules (first match wins):
 *  1. Explicit scheme: `ws://`/`wss://` are used as typed; `http://`/`https://`
 *     map to `ws://`/`wss://`. Anything else (ftp:, javascript: ...) is rejected.
 *  2. No scheme: the connection is `wss` when the page itself is served over
 *     https (browsers block `ws://` from secure pages, "mixed content"),
 *     otherwise `ws`. Pages that are not http(s) at all (Tauri, file:) count
 *     as plain http.
 *  3. Port: an explicit port always wins. A bare host without a port gets the
 *     server default 8790 when the page is plain http (dev / LAN use, where
 *     the server is run directly), and no port (=> 443) when the page is
 *     https, because a TLS deployment sits behind a reverse proxy. The one
 *     exception is the page's own host: typing the host this page came from
 *     reuses the page's port, so "same server as the web client" just works.
 *  4. Path: empty or `/` becomes `/ws`; any other explicit path is kept.
 *  5. IPv6: `[::1]:8790` or a bare `::1` / `fe80::1` (several colons, no
 *     brackets) is accepted and bracketed in the URL.
 *  6. `ws://` to a non-loopback host from an https page is rejected as mixed
 *     content, with a dedicated error so the UI can explain it.
 */
export const DEFAULT_PORT = 8790;
export const DEFAULT_PATH = "/ws";

export interface PageContext {
  protocol: string; // "http:" | "https:" | "tauri:" ...
  hostname: string;
  port: string; // "" when default
}

export type AddressError = "empty" | "invalid" | "scheme" | "mixed_content";

export interface ParsedAddress {
  /** Full URL to open, e.g. `wss://voice.example.com/ws`. */
  url: string;
  /** Short human label, e.g. `voice.example.com:8790`. */
  label: string;
  secure: boolean;
}

export type ParseResult = { ok: true; value: ParsedAddress } | { ok: false; error: AddressError };

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]", "::1"]);

export function pageContextFromLocation(loc: Pick<Location, "protocol" | "hostname" | "port"> = window.location): PageContext {
  return { protocol: loc.protocol, hostname: loc.hostname, port: loc.port };
}

/** The WebSocket URL of the server that served this page (rule: same origin). */
export function sameOriginUrl(page: PageContext): string {
  const secure = page.protocol === "https:";
  const host = page.hostname.includes(":") ? `[${page.hostname}]` : page.hostname;
  return `${secure ? "wss" : "ws"}://${host}${page.port ? `:${page.port}` : ""}${DEFAULT_PATH}`;
}

function isIpv6Bare(text: string): boolean {
  return !text.startsWith("[") && (text.match(/:/g)?.length ?? 0) >= 2;
}

export function parseServerAddress(input: string, page: PageContext): ParseResult {
  let text = input.trim();
  if (!text) return { ok: false, error: "empty" };

  const pageSecure = page.protocol === "https:";
  let scheme: "ws" | "wss" | null = null;

  const schemeMatch = /^([a-z][a-z0-9+.-]*):\/\//i.exec(text);
  if (schemeMatch) {
    const s = (schemeMatch[1] ?? "").toLowerCase();
    if (s === "ws" || s === "http") scheme = "ws";
    else if (s === "wss" || s === "https") scheme = "wss";
    else return { ok: false, error: "scheme" };
    text = text.slice(schemeMatch[0].length);
  } else if (/^[a-z][a-z0-9+.-]*:[^0-9:]/i.test(text) && !isIpv6Bare(text)) {
    // `javascript:alert(1)`, `mailto:x` - a scheme without slashes.
    return { ok: false, error: "scheme" };
  }

  // Split authority from path.
  let slash = text.search(/[/?#]/);
  if (slash < 0) slash = text.length;
  let authority = text.slice(0, slash);
  let path = text.slice(slash);
  path = path.replace(/[?#].*$/, "");
  if (authority.includes("@")) return { ok: false, error: "invalid" };

  if (isIpv6Bare(authority)) authority = `[${authority}]`;
  const hostPort = /^(\[[0-9a-f:.]+\]|[^:[\]\s/]+)(?::(\d{1,5}))?$/i.exec(authority);
  if (!hostPort) return { ok: false, error: "invalid" };
  const host = (hostPort[1] ?? "").toLowerCase();
  let port = hostPort[2] ?? "";
  if (port && (Number(port) < 1 || Number(port) > 65535)) return { ok: false, error: "invalid" };
  if (!host || host.startsWith(".") || host.endsWith(".") || host.includes("..")) return { ok: false, error: "invalid" };

  const explicitScheme = scheme !== null;
  const secure = (scheme ?? (pageSecure ? "wss" : "ws")) === "wss";

  if (!secure && pageSecure && !LOOPBACK.has(host)) return { ok: false, error: "mixed_content" };

  if (!port && !explicitScheme) {
    const pageHost = page.hostname.includes(":") ? `[${page.hostname}]` : page.hostname;
    if (host === pageHost.toLowerCase() && page.port) port = page.port;
    else if (!pageSecure) port = String(DEFAULT_PORT);
  }

  if (path === "" || path === "/") path = DEFAULT_PATH;

  let url: URL;
  try {
    url = new URL(`${secure ? "wss" : "ws"}://${host}${port ? `:${port}` : ""}${path}`);
  } catch {
    return { ok: false, error: "invalid" };
  }
  const label = url.host + (url.pathname === DEFAULT_PATH ? "" : url.pathname);
  return { ok: true, value: { url: url.toString(), label, secure } };
}

// ---------------------------------------------------------------- TeamSpeak

export const TS_DEFAULT_PORT = 9987;

export type TsParseResult = { ok: true; value: { address: string; host: string; label: string } } | { ok: false; error: "empty" | "invalid" };

/**
 * `host`, `host:port`, `[v6]:port` or a bare IPv6 address, optionally with a
 * `ts3server://` prefix. The port defaults to 9987. `address` is what the
 * native client gets (`host:port`, IPv6 bracketed); `host` has no brackets.
 */
export function parseTeamSpeakAddress(input: string): TsParseResult {
  let text = input.trim().replace(/^ts3?server:\/\//i, "");
  text = text.replace(/[/?#].*$/, "");
  if (!text) return { ok: false, error: "empty" };
  if (isIpv6Bare(text)) text = `[${text}]`;
  const m = /^(\[[0-9a-f:.]+\]|[^:[\]\s@/]+)(?::(\d{1,5}))?$/i.exec(text);
  if (!m) return { ok: false, error: "invalid" };
  const bracketed = (m[1] ?? "").toLowerCase();
  const port = m[2] ? Number(m[2]) : TS_DEFAULT_PORT;
  if (port < 1 || port > 65535 || bracketed.startsWith(".") || bracketed.endsWith(".") || bracketed.includes("..")) {
    return { ok: false, error: "invalid" };
  }
  const host = bracketed.replace(/^\[|\]$/g, "");
  const label = port === TS_DEFAULT_PORT ? host : `${host}:${port}`;
  return { ok: true, value: { address: `${bracketed}:${port}`, host, label } };
}

// ------------------------------------------------------------------ HTTP side

/**
 * The HTTP(S) origin of the server behind a `ws(s)://host[:port]/ws` URL: where
 * uploads are `PUT` and attachments are fetched from.
 */
export function httpOriginFromWsUrl(wsUrl: string): string | null {
  try {
    const u = new URL(wsUrl);
    if (u.protocol !== "ws:" && u.protocol !== "wss:") return null;
    return `${u.protocol === "wss:" ? "https" : "http"}://${u.host}`;
  } catch {
    return null;
  }
}

/** Joins a server-relative path (`/files/...`) to the server's HTTP origin. */
export function absoluteUrl(origin: string | null, path: string): string {
  if (/^https?:\/\//i.test(path)) return path;
  if (!origin) return path;
  return origin.replace(/\/+$/, "") + (path.startsWith("/") ? path : `/${path}`);
}
