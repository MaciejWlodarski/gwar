import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Event } from "../proto/Event";
import type { Welcome } from "../proto/Welcome";
import { imageType, MAX_IMAGE_BYTES, MAX_IMAGES, MAX_TOTAL_BYTES, TsImages, useTsImages } from "./ts-images";

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
const bytes = (text: string) => new TextEncoder().encode(text);
const avatar = (n: number) => `a${n.toString(16).padStart(40, "0")}`;
const AVATAR = avatar(0xabc);
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));
/** Lets a load that answers at once land. */
const settle = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};
/** A PNG of `size` bytes. */
const png = (size: number) => {
  const out = new Uint8Array(size);
  out.set(PNG);
  return out;
};

const welcome = (d: { server?: object; channels?: object[]; clients?: object[] }) =>
  ({ server: { name: "TS", ...d.server }, channels: d.channels ?? [], clients: d.clients ?? [] }) as unknown as Welcome;
const event = (ev: string, d: object) => ({ ev, d }) as unknown as Event;
const client = (id: number, avatar?: string) => event("client.updated", { id, avatar });

describe("TeamSpeak images", () => {
  let made = 0;
  const live: TsImages[] = [];
  const make = (fetch: ConstructorParameters<typeof TsImages>[0]) => {
    const images = new TsImages(fetch);
    live.push(images);
    return images;
  };
  beforeEach(() => {
    made = 0;
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:gwar/${++made}`);
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    useTsImages.setState({ urls: {} });
  });
  afterEach(() => {
    for (const images of live.splice(0)) images.clear();
    vi.restoreAllMocks();
  });

  it("takes PNG, JPEG, GIF and WebP by their first bytes, and nothing else", () => {
    expect(imageType(PNG)).toBe("image/png");
    expect(imageType(new Uint8Array([0xff, 0xd8, 0xff, 0xe0]))).toBe("image/jpeg");
    expect(imageType(bytes("GIF89a\x01\x00"))).toBe("image/gif");
    expect(imageType(bytes("GIF87a\x01\x00"))).toBe("image/gif");
    expect(imageType(bytes("RIFF\x1a\x00\x00\x00WEBPVP8 "))).toBe("image/webp");
    for (const refused of ['<svg xmlns="http://www.w3.org/2000/svg"/>', "<?xml version='1.0'?><svg/>", "<html><script>alert(1)</script>", "RIFF\x1a\x00\x00\x00WAVEfmt ", "GIF88a", ""]) {
      expect(imageType(bytes(refused))).toBeNull();
    }
  });

  it("loads what the welcome and events name, each once, into a blob: URL of its type", async () => {
    const fetch = vi.fn(async (_token: string) => PNG.slice().buffer);
    const images = make(fetch);
    images.welcome(welcome({
      server: { icon: "i1001", badges: [{ kind: "server", id: 6, name: "A", sort: 0, icon: "i1002" }, { kind: "server", id: 7, name: "B", sort: 0 }] },
      channels: [{ id: 1, icon: "i1003" }, { id: 2, icon: null }],
      clients: [{ id: 5, avatar: AVATAR }, { id: 6 }],
    }));
    images.event(event("channel.created", { id: 3, icon: "i1001" }));
    images.event(event("client.left", { client: 9, reason: { kind: "quit" } }));
    await flush();
    expect(fetch.mock.calls.map((c) => c[0]).sort()).toEqual([AVATAR, "i1001", "i1002", "i1003"]);
    expect(Object.keys(useTsImages.getState().urls).sort()).toEqual([AVATAR, "i1001", "i1002", "i1003"]);
    const blob = vi.mocked(URL.createObjectURL).mock.calls[0]?.[0] as Blob;
    expect(blob.type).toBe("image/png");
  });

  it("refuses what is no image, too large, or no token, and asks again only once it is named again", async () => {
    const replies: Record<string, ArrayBuffer | Uint8Array | null> = {
      i1: bytes("<svg onload=alert(1)>"),
      i2: new Uint8Array(MAX_IMAGE_BYTES + 1).fill(0xff),
      i3: null,
    };
    const fetch = vi.fn(async (token: string) => replies[token] ?? null);
    const images = make(fetch);
    const named = ["i1", "i2", "i3", "https://evil.test/a.png", "blob:x", "javascript:alert(1)", "I1", `a${"0".repeat(41)}`];
    images.welcome(welcome({ channels: named.map((icon, id) => ({ id, icon })) }));
    await flush();
    expect(fetch.mock.calls.map((c) => c[0])).toEqual(["i1", "i2", "i3"]);
    expect(useTsImages.getState().urls).toEqual({});
    expect(URL.createObjectURL).not.toHaveBeenCalled();
    // Still named: not asked for again.
    images.event(event("channel.updated", { id: 9, icon: "i1" }));
    await flush();
    expect(fetch).toHaveBeenCalledTimes(3);
    // No longer named, then named again: asked for once more.
    replies.i3 = PNG;
    images.event(event("channel.deleted", { channel: 2 }));
    images.event(event("channel.updated", { id: 2, icon: "i3" }));
    await flush();
    expect(useTsImages.getState().urls).toEqual({ i3: "blob:gwar/1" });
  });

  it("keeps only the images named now: a replaced avatar or a client who left frees its URL", async () => {
    const images = make(async () => PNG);
    images.welcome(welcome({ clients: [{ id: 1, avatar: avatar(1) }, { id: 2, avatar: avatar(2) }] }));
    await flush();
    expect(Object.keys(useTsImages.getState().urls).sort()).toEqual([avatar(1), avatar(2)]);
    // Client 1 replaces their avatar more times than MAX_IMAGES: one URL at a time, the old ones revoked.
    const times = MAX_IMAGES + 10;
    for (let n = 3; n < 3 + times; n++) {
      images.event(client(1, avatar(n)));
      await settle();
    }
    const last = avatar(2 + times);
    expect(Object.keys(useTsImages.getState().urls).sort()).toEqual([avatar(2), last].sort());
    expect(made).toBe(2 + times);
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(times);
    // Client 2 leaves; one without an avatar any more frees theirs too.
    images.event(event("client.left", { client: 2, reason: { kind: "quit" } }));
    images.event(client(1));
    expect(useTsImages.getState().urls).toEqual({});
    // And new images still load.
    images.event(client(3, avatar(3)));
    await flush();
    expect(Object.keys(useTsImages.getState().urls)).toEqual([avatar(3)]);
  });

  it("keeps at most MAX_IMAGES and MAX_TOTAL_BYTES, and loads the rest as room is made", async () => {
    const fetch = vi.fn(async (token: string) => (token.startsWith("a") ? png(MAX_IMAGE_BYTES) : PNG));
    const images = make(fetch);
    const icons = Array.from({ length: MAX_IMAGES + 5 }, (_, i) => ({ id: i, icon: `i${i + 1000}` }));
    images.welcome(welcome({ channels: icons }));
    await flush();
    expect(fetch).toHaveBeenCalledTimes(MAX_IMAGES);
    expect(Object.keys(useTsImages.getState().urls)).toHaveLength(MAX_IMAGES);
    // Five channels go: the five left out load.
    for (let id = 0; id < 5; id++) images.event(event("channel.deleted", { channel: id }));
    await flush();
    expect(fetch).toHaveBeenCalledTimes(MAX_IMAGES + 5);
    expect(Object.keys(useTsImages.getState().urls)).toHaveLength(MAX_IMAGES);

    // Bytes: as many full-size avatars as fit, one more waits, and loads once one leaves.
    images.clear();
    fetch.mockClear();
    const fit = Math.floor(MAX_TOTAL_BYTES / MAX_IMAGE_BYTES);
    const clients = Array.from({ length: fit + 1 }, (_, id) => ({ id, avatar: avatar(id) }));
    images.welcome(welcome({ clients }));
    await flush();
    expect(Object.keys(useTsImages.getState().urls)).toHaveLength(fit);
    const waiting = clients.find((c) => !(c.avatar in useTsImages.getState().urls))!;
    images.event(client(waiting.id === 0 ? 1 : 0));
    await flush();
    expect(useTsImages.getState().urls[waiting.avatar]).toBeDefined();
    expect(Object.keys(useTsImages.getState().urls)).toHaveLength(fit);
  });

  it("forgets them all when the session ends, and drops a load that finishes after", async () => {
    let release: (b: Uint8Array) => void = () => {};
    const images = make(async (token) => (token === "i1001" ? PNG : new Promise((resolve) => (release = resolve))));
    images.welcome(welcome({ server: { icon: "i1001" }, channels: [{ id: 1, icon: "i1002" }] }));
    await flush();
    expect(Object.keys(useTsImages.getState().urls)).toEqual(["i1001"]);
    images.clear();
    expect(useTsImages.getState().urls).toEqual({});
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:gwar/1");
    release(PNG);
    await flush();
    expect(useTsImages.getState().urls).toEqual({});
  });
});
