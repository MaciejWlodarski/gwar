/** TLS by default; plaintext LAN connections require an explicit desktop opt-in. */
export const DEFAULT_PORT = 8790;
export const DEFAULT_PATH = "/ws";

export interface PageContext {
  protocol: string; // "http:" | "https:" | "tauri:" ...
  hostname: string;
  port: string; // "" when default
}

export type AddressError = "empty" | "invalid" | "scheme" | "mixed_content" | "insecure";

export interface ParsedAddress {
  /** Full URL to open, e.g. `wss://voice.example.com/ws`. */
  url: string;
  /** Short human label, e.g. `voice.example.com:8790`. */
  label: string;
  secure: boolean;
}

export type ParseResult = { ok: true; value: ParsedAddress } | { ok: false; error: AddressError };

export function isLoopbackHost(host: string): boolean {
  host = host.toLowerCase().replace(/\.+$/, "");
  return host === "localhost" || host === "[::1]" || host === "::1" || (/^127(?:\.\d{1,3}){3}$/.test(host) && host.split(".").every((part) => Number(part) <= 255));
}

/** Plaintext to another machine: it can be impersonated or taken over. Loopback stays on this machine. */
export function isPlaintextRemote(endpoint: string): boolean {
  const url = new URL(endpoint);
  return url.protocol === "ws:" && !isLoopbackHost(url.hostname);
}

export function pageContextFromLocation(loc: Pick<Location, "protocol" | "hostname" | "port"> = window.location): PageContext {
  return { protocol: loc.protocol, hostname: loc.hostname, port: loc.port };
}

/** The WebSocket URL of the server that served this page (rule: same origin). */
export function sameOriginUrl(page: PageContext): string {
  const secure = page.protocol === "https:";
  const host = page.hostname.includes(":") ? `[${page.hostname}]` : page.hostname;
  return `${secure ? "wss" : "ws"}://${host}${page.port ? `:${page.port}` : ""}${DEFAULT_PATH}`;
}

/** The desktop app's own page rather than a web page served over http(s). */
function isAppPage(page: PageContext): boolean {
  return (page.protocol !== "http:" && page.protocol !== "https:") || page.hostname === "tauri.localhost";
}

function isIpv6Bare(text: string): boolean {
  return !text.startsWith("[") && (text.match(/:/g)?.length ?? 0) >= 2;
}

export function parseServerAddress(input: string, page: PageContext, insecureLan = false): ParseResult {
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
  if (/[?#]/.test(path) || /[\s\\]/.test(text)) return { ok: false, error: "invalid" };
  if (authority.includes("@")) return { ok: false, error: "invalid" };

  if (isIpv6Bare(authority)) authority = `[${authority}]`;
  const hostPort = /^(\[[0-9a-f:.]+\]|[^:[\]\s/]+)(?::(\d{1,5}))?$/i.exec(authority);
  if (!hostPort) return { ok: false, error: "invalid" };
  const host = (hostPort[1] ?? "").toLowerCase().replace(/\.+$/, "");
  let port = hostPort[2] ?? "";
  if (port && (Number(port) < 1 || Number(port) > 65535)) return { ok: false, error: "invalid" };
  if (!host || host.startsWith(".") || host.endsWith(".") || host.includes("..")) return { ok: false, error: "invalid" };

  const explicitScheme = scheme !== null;
  const pageHost = page.hostname.includes(":") ? `[${page.hostname}]` : page.hostname;
  const isPageHost = host === pageHost.toLowerCase();
  const loopback = isLoopbackHost(host);
  const defaultScheme = loopback || (isAppPage(page) && insecureLan) ? "ws" : "wss";
  const secure = (scheme ?? defaultScheme) === "wss";
  if (!secure && !loopback && !(isAppPage(page) && insecureLan)) {
    return { ok: false, error: pageSecure ? "mixed_content" : "insecure" };
  }

  if (!port && !explicitScheme) {
    if (isPageHost && page.port) port = page.port;
    else if (!secure) port = String(DEFAULT_PORT);
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

export type FileMethod = "GET" | "PUT";

/** Only the two file routes. Decode before checking, never normalize hostile paths. */
export function filePath(path: string, method: FileMethod = "GET"): string {
  if (path.length > 2048 || /[\\\s#]/.test(path)) throw new Error("Invalid file path");
  let decoded: string;
  try { decoded = decodeURIComponent(path); } catch { throw new Error("Invalid file path"); }
  // Names may contain "%" or "..", but no segment may be a dot segment, and no character may be a control one.
  // eslint-disable-next-line no-control-regex
  if (/[\\\x00-\x1f\x7f]/.test(decoded) || decoded.split("/").some((seg) => seg === "." || seg === "..")) throw new Error("Invalid file path");
  const valid = method === "GET"
    ? /^\/files\/[A-Za-z0-9_-]{1,64}\/[^/?#]+$/.test(decoded)
    : /^\/api\/files\/[A-Za-z0-9_-]{1,64}\?token=[A-Za-z0-9_-]{1,128}$/.test(decoded);
  if (!valid) throw new Error("Invalid file path");
  return path;
}

/** Resolves a file path against the locally selected server origin. */
export function absoluteUrl(origin: string | null, path: string, method: FileMethod = "GET"): string {
  filePath(path, method);
  if (!origin) throw new Error("No connected server");
  const base = new URL(origin);
  if (!["http:", "https:"].includes(base.protocol) || base.username || base.password || base.pathname !== "/" || base.search || base.hash) {
    throw new Error("Invalid server origin");
  }
  const url = new URL(path, base);
  if (url.origin !== base.origin) throw new Error("Invalid file origin");
  return url.href;
}
