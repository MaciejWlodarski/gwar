/**
 * Icons and avatars of the TeamSpeak server (desktop app only). The native adapter fetches them
 * through TeamSpeak's file transfer, checks them and names each by a token in the shapes it sends
 * (`ServerInfo.icon`, `Badge.icon`, `Channel.icon`, `Client.avatar`); see
 * `gwar_client::teamspeak::images`. Here the TeamSpeak connection asks it for the bytes of every
 * token the session names now (`ts_image`), checks them again, and shows them through `blob:` URLs
 * of this origin: never a remote URL, and only PNG, JPEG, GIF or WebP (no SVG, nothing that runs).
 */
import { create } from "zustand";
import type { Event } from "../proto/Event";
import type { Welcome } from "../proto/Welcome";
import type { ServerInfo } from "../proto/ServerInfo";
import { imageToken } from "./schema";

/** Images kept (or loading) per connection, at most (the adapter keeps as many). */
export const MAX_IMAGES = 2048;
/** The largest image accepted: a TeamSpeak avatar (300 KiB). */
export const MAX_IMAGE_BYTES = 300 * 1024;
/** All images kept per connection together, in bytes (the adapter's cache is as large). */
export const MAX_TOTAL_BYTES = 32 * 1024 * 1024;

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

function serverTokens(server: ServerInfo | undefined): Set<string> {
  const out = new Set<string>();
  if (!server) return out;
  if (server.icon) out.add(server.icon);
  for (const b of server.badges ?? []) if (b.icon) out.add(b.icon);
  return out;
}

function setToken(map: Map<number, string>, id: number, token: string | null | undefined): void {
  if (token) map.set(id, token);
  else map.delete(id);
}

/**
 * The images of one TeamSpeak connection, through `fetch` (the native `ts_image`), in
 * `useTsImages`. It follows the tokens the server, its badges, channels and clients name now
 * (from the Welcome and the events), and keeps exactly those: an image no longer named has its
 * URL revoked, at most MAX_IMAGES and MAX_TOTAL_BYTES are kept, and one that failed is not asked
 * for again while it stays named. A token names one image for good (an icon id is TeamSpeak's
 * checksum of the file; an avatar token covers its owner and hash).
 */
export class TsImages {
  private server = new Set<string>();
  private channels = new Map<number, string>();
  private clients = new Map<number, string>();
  /** The tokens named now. */
  private named = new Set<string>();
  private kept = new Map<string, { url: string; size: number }>();
  private bytes = 0;
  private loading = new Set<string>();
  /** Named tokens that could not be loaded. */
  private failed = new Set<string>();
  /** Named tokens that did not fit MAX_TOTAL_BYTES, by size: loaded again once they would. */
  private deferred = new Map<string, number>();
  private generation = 0;

  constructor(private readonly fetch: (token: string) => Promise<ArrayBuffer | Uint8Array | null>) {}

  /** A (re)joined session: everything it names. */
  welcome(welcome: Welcome): void {
    this.server = serverTokens(welcome.server);
    this.channels = new Map();
    for (const c of welcome.channels) setToken(this.channels, c.id, c.icon);
    this.clients = new Map();
    for (const c of welcome.clients) setToken(this.clients, c.id, c.avatar);
    this.reconcile();
  }

  /** An event of the session: what it names now. */
  event(event: Event): void {
    switch (event.ev) {
      case "server.updated":
        this.server = serverTokens(event.d);
        break;
      case "channel.created":
      case "channel.updated":
        setToken(this.channels, event.d.id, event.d.icon);
        break;
      case "channel.deleted":
      case "channel.hidden":
        this.channels.delete(event.d.channel);
        break;
      case "client.joined":
      case "client.updated":
        setToken(this.clients, event.d.id, event.d.avatar);
        break;
      case "client.left":
        this.clients.delete(event.d.client);
        break;
      default:
        return;
    }
    this.reconcile();
  }

  /** Forgets every image and revokes its URL; loads under way are dropped (the session ended). */
  clear(): void {
    this.generation++;
    this.server = new Set();
    this.channels = new Map();
    this.clients = new Map();
    this.named = new Set();
    this.loading.clear();
    this.failed.clear();
    this.deferred.clear();
    const kept = [...this.kept.values()];
    this.kept.clear();
    this.bytes = 0;
    useTsImages.setState({ urls: {} });
    for (const image of kept) URL.revokeObjectURL(image.url);
  }

  /** Drops what is no longer named, then loads what is and fits. */
  private reconcile(): void {
    const named = new Set([...this.server, ...this.channels.values(), ...this.clients.values()]);
    this.named = named;
    const gone = [...this.kept.keys()].filter((token) => !named.has(token));
    if (gone.length > 0) {
      const urls = { ...useTsImages.getState().urls };
      for (const token of gone) {
        const image = this.kept.get(token)!;
        this.kept.delete(token);
        this.bytes -= image.size;
        delete urls[token];
        URL.revokeObjectURL(image.url);
      }
      useTsImages.setState({ urls });
    }
    for (const token of this.failed) if (!named.has(token)) this.failed.delete(token);
    for (const token of this.deferred.keys()) if (!named.has(token)) this.deferred.delete(token);
    this.start();
  }

  private start(): void {
    for (const token of this.named) {
      if (this.kept.size + this.loading.size >= MAX_IMAGES) return;
      if (this.kept.has(token) || this.loading.has(token) || this.failed.has(token)) continue;
      const size = this.deferred.get(token);
      if (size !== undefined && this.bytes + size > MAX_TOTAL_BYTES) continue;
      if (!imageToken(token)) {
        this.failed.add(token);
        continue;
      }
      this.loading.add(token);
      void this.load(token, this.generation);
    }
  }

  private async load(token: string, generation: number): Promise<void> {
    let data: ArrayBuffer | Uint8Array | null;
    try {
      data = await this.fetch(token);
    } catch {
      data = null;
    }
    if (generation !== this.generation) return;
    this.loading.delete(token);
    const bytes = data instanceof Uint8Array ? data : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
    const type = bytes && bytes.length <= MAX_IMAGE_BYTES ? imageType(bytes) : null;
    if (!this.named.has(token)) {
      // No longer named while it loaded.
    } else if (!bytes || !type) {
      this.failed.add(token);
    } else if (this.bytes + bytes.length > MAX_TOTAL_BYTES) {
      this.deferred.set(token, bytes.length);
    } else {
      this.deferred.delete(token);
      const url = URL.createObjectURL(new Blob([bytes as Uint8Array<ArrayBuffer>], { type }));
      this.kept.set(token, { url, size: bytes.length });
      this.bytes += bytes.length;
      useTsImages.setState((s) => ({ urls: { ...s.urls, [token]: url } }));
      return;
    }
    // A place for another one.
    this.start();
  }
}

/** The `blob:` URL of an image token, once it is loaded. */
export function useTsImage(token: string | null | undefined): string | null {
  return useTsImages((s) => (token ? (s.urls[token] ?? null) : null));
}
