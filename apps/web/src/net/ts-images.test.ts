import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Welcome } from "../proto/Welcome";
import { imageTokens, imageType, MAX_IMAGE_BYTES, MAX_IMAGES, TsImages, useTsImages } from "./ts-images";

const PNG = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
const bytes = (text: string) => new TextEncoder().encode(text);
const AVATAR = `a${"0123456789".repeat(4)}`;
const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("TeamSpeak images", () => {
  let made = 0;
  beforeEach(() => {
    made = 0;
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:gwar/${++made}`);
    vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    useTsImages.setState({ urls: {} });
  });
  afterEach(() => vi.restoreAllMocks());

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

  it("finds the tokens a welcome or an event names", () => {
    const welcome = {
      server: { icon: "i1001", badges: [{ kind: "server", id: 6, name: "A", sort: 0, icon: "i1002" }, { kind: "server", id: 7, name: "B", sort: 0 }] },
      channels: [{ icon: "i1003" }, { icon: null }],
      clients: [{ avatar: AVATAR }, {}],
    } as unknown as Welcome;
    expect([...imageTokens({ welcome })].sort()).toEqual([AVATAR, "i1001", "i1002", "i1003"]);
    expect([...imageTokens({ event: { ev: "channel.updated", d: { icon: "i5000" } } as never })]).toEqual(["i5000"]);
    expect([...imageTokens({ event: { ev: "client.joined", d: { avatar: AVATAR } } as never })]).toEqual([AVATAR]);
    expect([...imageTokens({ event: { ev: "client.left", d: { client: 1, reason: "quit" } } as never })]).toEqual([]);
  });

  it("loads each token once into a blob: URL of its type", async () => {
    const fetch = vi.fn(async () => PNG.slice().buffer);
    const images = new TsImages(fetch);
    images.request(["i1001", AVATAR, "i1001"]);
    images.request(["i1001"]);
    await flush();
    expect(fetch.mock.calls.map((c) => c[0])).toEqual(["i1001", AVATAR]);
    expect(useTsImages.getState().urls).toEqual({ i1001: "blob:gwar/1", [AVATAR]: "blob:gwar/2" });
    const blob = vi.mocked(URL.createObjectURL).mock.calls[0][0] as Blob;
    expect(blob.type).toBe("image/png");
  });

  it("refuses what is no image, too large, or no token", async () => {
    const replies: Record<string, ArrayBuffer | Uint8Array | null> = {
      i1: bytes("<svg onload=alert(1)>"),
      i2: new Uint8Array(MAX_IMAGE_BYTES + 1).fill(0xff),
      i3: null,
    };
    const fetch = vi.fn(async (token: string) => replies[token] ?? null);
    const images = new TsImages(fetch);
    images.request(["i1", "i2", "i3", "https://evil.test/a.png", "blob:x", "javascript:alert(1)", "I1", `a${"0".repeat(41)}`]);
    await flush();
    expect(fetch.mock.calls.map((c) => c[0])).toEqual(["i1", "i2", "i3"]);
    expect(useTsImages.getState().urls).toEqual({});
    expect(URL.createObjectURL).not.toHaveBeenCalled();
    // A token that failed is asked for again when it is named again.
    replies.i3 = PNG;
    images.request(["i3"]);
    await flush();
    expect(useTsImages.getState().urls).toEqual({ i3: "blob:gwar/1" });
  });

  it("keeps at most MAX_IMAGES and forgets them all when the connection closes", async () => {
    let fetched = 0;
    const images = new TsImages(async () => {
      fetched++;
      return PNG;
    });
    images.request(Array.from({ length: MAX_IMAGES + 5 }, (_, i) => `i${i + 1000}`));
    await flush();
    expect(fetched).toBe(MAX_IMAGES);
    images.clear();
    expect(useTsImages.getState().urls).toEqual({});
    expect(URL.revokeObjectURL).toHaveBeenCalledTimes(MAX_IMAGES);
  });

  it("drops a load that finishes after the connection closed", async () => {
    let release: (b: Uint8Array) => void = () => {};
    const images = new TsImages(() => new Promise((resolve) => (release = resolve)));
    images.request(["i1001"]);
    images.clear();
    release(PNG);
    await flush();
    expect(useTsImages.getState().urls).toEqual({});
  });
});
