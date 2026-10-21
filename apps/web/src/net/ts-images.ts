/**
 * Icons and avatars of the TeamSpeak server (desktop app only). The native adapter fetches them
 * through TeamSpeak's file transfer, checks them and names each by a token in the shapes it sends
 * (`ServerInfo.icon`, `Badge.icon`, `Channel.icon`, `Client.avatar`); see
 * `gwar_client::teamspeak::images`. Here the TeamSpeak connection asks it for the bytes of every
 * token it sees (`ts_image`), checks them again, and shows them through `blob:` URLs of this
 * origin: never a remote URL, and only PNG, JPEG, GIF or WebP (no SVG, nothing that runs).
 */
import { create } from "zustand";
import type { Event } from "../proto/Event";
import type { Welcome } from "../proto/Welcome";
import type { ServerInfo } from "../proto/ServerInfo";
import { imageToken } from "./schema";

/** Image tokens kept per connection, at most (the adapter keeps as many). */
export const MAX_IMAGES = 2048;
/** The largest image accepted: a TeamSpeak avatar (300 KiB). */
export const MAX_IMAGE_BYTES = 300 * 1024;

/** `blob:` URLs of the TeamSpeak connection's images, by token. */
export const useTsImages = create<{ urls: Readonly<Record<string, string>> }>(() => ({ urls: {} }));

/** The image type of `bytes` by their first bytes, or null for anything else. */
export function imageType(bytes: Uint8Array): "image/png" | "image/jpeg" | "image/gif" | "image/webp" | null {
  const starts = (...b: number[]) => b.every((x, i) => bytes[i] === x);
  if (starts(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)) return "image/png";
  if (starts(0xff, 0xd8, 0xff)) return "image/jpeg";
  if (starts(0x47, 0x49, 0x46, 0x38) && (bytes[4] === 0x37 || bytes[4] === 0x39) && bytes[5] === 0x61) return "image/gif";
  if (bytes.length >= 12 && starts(0x52, 0x49, 0x46, 0x46) && bytes[8] === 0x57 && bytes[9] === 0x45 && bytes[10] === 0x42 && bytes[11] === 0x50) return "image/webp";
  return null;
}

function serverTokens(server: ServerInfo | undefined, out: Set<string>): void {
  if (!server) return;
  if (server.icon) out.add(server.icon);
  for (const b of server.badges ?? []) if (b.icon) out.add(b.icon);
}

/** The image tokens a Welcome or an event names. */
export function imageTokens(frame: { welcome: Welcome } | { event: Event }): Set<string> {
  const out = new Set<string>();
  if ("welcome" in frame) {
    const w = frame.welcome;
    serverTokens(w.server, out);
    for (const c of w.channels) if (c.icon) out.add(c.icon);
    for (const c of w.clients) if (c.avatar) out.add(c.avatar);
    return out;
  }
  const e = frame.event;
  if (e.ev === "server.updated") serverTokens(e.d, out);
  else if ((e.ev === "channel.created" || e.ev === "channel.updated") && e.d.icon) out.add(e.d.icon);
  else if ((e.ev === "client.joined" || e.ev === "client.updated") && e.d.avatar) out.add(e.d.avatar);
  return out;
}

/**
 * Loads each token once, through `fetch` (the native `ts_image`), into `useTsImages`. A token
 * names one image for good (an icon id is TeamSpeak's checksum of the file; an avatar token
 * covers its owner and hash), so a loaded one is never asked for again.
 */
export class TsImages {
  private seen = new Set<string>();
  private generation = 0;

  constructor(private readonly fetch: (token: string) => Promise<ArrayBuffer | Uint8Array | null>) {}

  request(tokens: Iterable<string>): void {
    for (const token of tokens) {
      if (this.seen.has(token) || this.seen.size >= MAX_IMAGES || !imageToken(token)) continue;
      this.seen.add(token);
      void this.load(token, this.generation);
    }
  }

  /** Forgets every image and revokes its URL (the connection closed). */
  clear(): void {
    this.generation++;
    this.seen.clear();
    const urls = useTsImages.getState().urls;
    useTsImages.setState({ urls: {} });
    for (const url of Object.values(urls)) URL.revokeObjectURL(url);
  }

  private async load(token: string, generation: number): Promise<void> {
    let data: ArrayBuffer | Uint8Array | null;
    try {
      data = await this.fetch(token);
    } catch {
      data = null;
    }
    if (generation !== this.generation) return;
    const bytes = data instanceof Uint8Array ? data : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
    const type = bytes && bytes.length <= MAX_IMAGE_BYTES ? imageType(bytes) : null;
    if (!bytes || !type) {
      // Gone (evicted, or the connection moved on): asked for again if it is named again.
      this.seen.delete(token);
      return;
    }
    const url = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type }));
    useTsImages.setState((s) => ({ urls: { ...s.urls, [token]: url } }));
  }
}

/** The `blob:` URL of an image token, once it is loaded. */
export function useTsImage(token: string | null | undefined): string | null {
  return useTsImages((s) => (token ? (s.urls[token] ?? null) : null));
}
