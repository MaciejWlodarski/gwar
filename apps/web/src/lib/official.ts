/**
 * The one official web app, hosted by the project: people open it and connect to
 * any self-hosted Gwar server from there. Invite links point at it (the desktop
 * app has no web address of its own, `location.origin` is `tauri://...` there).
 *
 * Change it here only (and `OFFICIAL_WEB_ORIGIN` in vc-server).
 */
export const OFFICIAL_WEB_ORIGIN = "https://gwar.maciejwlodarski.com";

/**
 * Gwar Connect, the optional account service (docs/connect.md). nginx serves it
 * under /connect on the official host. Builds and tests can point elsewhere with
 * `VITE_CONNECT_URL` (e.g. a local `gwar-connect`).
 */
export const CONNECT_URL: string =
  (import.meta.env?.VITE_CONNECT_URL as string | undefined)?.replace(/\/+$/, "") || `${OFFICIAL_WEB_ORIGIN}/connect`;

/**
 * The public address of the web app: the page's own origin when it is served
 * over http(s), the official one otherwise (the desktop app, file:).
 */
export function webOrigin(loc: Pick<Location, "protocol" | "origin"> = window.location): string {
  return /^https?:$/i.test(loc.protocol) ? loc.origin : OFFICIAL_WEB_ORIGIN;
}
