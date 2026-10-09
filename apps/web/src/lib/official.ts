/**
 * The one official web app, hosted by the project: people open it and connect to
 * any self-hosted Gwar server from there. Invite links point at it (the desktop
 * app has no web address of its own, `location.origin` is `tauri://...` there).
 *
 * Moves to the Gwar domain later; change it here only.
 */
export const OFFICIAL_WEB_ORIGIN = "https://voice.maciejwlodarski.com";

/**
 * The public address of the web app: the page's own origin when it is served
 * over http(s), the official one otherwise (the desktop app, file:).
 */
export function webOrigin(loc: Pick<Location, "protocol" | "origin"> = window.location): string {
  return /^https?:$/i.test(loc.protocol) ? loc.origin : OFFICIAL_WEB_ORIGIN;
}
