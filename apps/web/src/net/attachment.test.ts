import { afterEach, describe, expect, it, vi } from "vitest";
import { verifiedAttachmentBlob, verifiedAttachmentUrl } from "./attachment";
import { sha256 } from "./message-signature";

afterEach(() => vi.restoreAllMocks());
describe("attachment bytes", () => {
  for (const mime of ["image/png", "video/mp4", "audio/ogg", "text/plain"]) it(`never gives mismatching ${mime} bytes an object URL`, async () => {
    const bytes = new TextEncoder().encode("hello");
    const att = { id: "f", name: "file", mime, size: bytes.length, url: "/files/f", sha256: await sha256(bytes) };
    const create = vi.spyOn(URL, "createObjectURL");
    await expect(verifiedAttachmentUrl(att, att.url, async () => new TextEncoder().encode("other"))).rejects.toThrow("hash mismatch");
    expect(create).not.toHaveBeenCalled();
  });
  it("returns only matching bytes and uses the signed media type", async () => {
    const bytes = new TextEncoder().encode("hello");
    const att = { id: "f", name: "file", mime: "text/plain", size: 5, url: "/files/f", sha256: await sha256(bytes) };
    const blob = await verifiedAttachmentBlob(att, att.url, async () => bytes);
    expect(await blob.text()).toBe("hello"); expect(blob.type).toBe("text/plain");
  });
});

it("bounds header and decoded image dimensions before creating a preview", async () => {
  const { imageDimensions, checkImageDimensions, boundedImageBlob } = await import("./attachment");
  const bytes = new Uint8Array(24); bytes.set([137, 80, 78, 71]); bytes.set([73, 72, 68, 82], 12);
  const view = new DataView(bytes.buffer); view.setUint32(16, 100_000); view.setUint32(20, 100_000);
  expect(imageDimensions(bytes)).toEqual([100_000, 100_000]);
  expect(() => checkImageDimensions(4096, 4096)).toThrow();
  expect(() => checkImageDimensions(0, 1)).toThrow();
  expect(() => checkImageDimensions(4000, 4000)).not.toThrow();
  const decode = vi.fn(); vi.stubGlobal("createImageBitmap", decode);
  await expect(boundedImageBlob(new Blob([bytes], { type: "image/png" }))).rejects.toThrow("dimensions limit");
  expect(decode).not.toHaveBeenCalled();
  view.setUint32(16, 1); view.setUint32(20, 1);
  const close = vi.fn(); decode.mockResolvedValue({ width: 100_000, height: 1, close });
  await expect(boundedImageBlob(new Blob([bytes], { type: "image/png" }))).rejects.toThrow("dimensions limit");
  expect(close).toHaveBeenCalledOnce(); vi.unstubAllGlobals();
});

// Sizes real decoders would compute differently from the header (seen in Chromium and WebKit).
describe("image headers that decoders size differently", () => {
  const gif = (screen: [number, number], frames: Array<[number, number, number, number]>, trailer = true) => {
    const b: number[] = [...new TextEncoder().encode("GIF89a")];
    const u16 = (n: number) => b.push(n & 255, n >> 8);
    u16(screen[0]); u16(screen[1]); b.push(0x80, 0, 0, 0, 0, 0, 255, 255, 255);
    b.push(0x21, 0xf9, 4, 0, 0, 0, 0, 0); // graphic control extension
    for (const [left, top, w, h] of frames) { b.push(0x2c); u16(left); u16(top); u16(w); u16(h); b.push(0, 2, 2, 0x4c, 0x01, 0); }
    if (trailer) b.push(0x3b);
    return new Uint8Array(b);
  };
  const jpeg = (...frames: Array<[number, number]>) => {
    const b = [0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0];
    for (const [w, h] of frames) b.push(0xff, 0xc0, 0, 11, 8, h >> 8, h & 255, w >> 8, w & 255, 1, 1, 0x11, 0);
    b.push(0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x12, 0x34, 0xff, 0xd9);
    return new Uint8Array(b);
  };

  it("takes a GIF's screen only when every frame lies inside it", async () => {
    const { imageDimensions } = await import("./attachment");
    expect(imageDimensions(gif([10, 10], [[0, 0, 10, 10], [2, 2, 8, 8]]))).toEqual([10, 10]);
    // Chromium decodes this at 300x200.
    expect(() => imageDimensions(gif([10, 10], [[0, 0, 300, 200]]))).toThrow("outside");
    expect(() => imageDimensions(gif([10, 10], [[0, 0, 10, 10], [5, 0, 10, 10]]))).toThrow("outside");
    expect(() => imageDimensions(gif([10, 10], [[0, 5, 10, 10]]))).toThrow("outside");
    expect(() => imageDimensions(gif([10, 10], []))).toThrow("malformed");
    expect(() => imageDimensions(gif([10, 10], [[0, 0, 10, 10]], false))).toThrow("malformed");
  });

  it("takes a JPEG's size only from its single frame header", async () => {
    const { imageDimensions } = await import("./attachment");
    expect(imageDimensions(jpeg([300, 200]))).toEqual([300, 200]);
    // WebKit decodes this at the second header's size.
    expect(() => imageDimensions(jpeg([16, 16], [4000, 4000]))).toThrow("more than one frame header");
    expect(() => imageDimensions(jpeg())).toThrow("malformed");
    expect(() => imageDimensions(jpeg([16, 16]).subarray(0, 12))).toThrow("malformed");
  });

  it("reads a PNG's size only from IHDR in its place", async () => {
    const { imageDimensions } = await import("./attachment");
    const png = new Uint8Array(40); png.set([137, 80, 78, 71, 13, 10, 26, 10]);
    png.set(new TextEncoder().encode("CgBI"), 12); // Apple's variant, decoded by WebKit with IHDR later
    expect(() => imageDimensions(png)).toThrow("malformed");
  });

  it("reads a lossless WebP shorter than the other kinds' headers", async () => {
    const { imageDimensions } = await import("./attachment");
    // `cwebp -lossless -z 9` of a transparent 1x1 PNG: 28 bytes.
    const webp = Uint8Array.from(atob("UklGRhQAAABXRUJQVlA4TAgAAAAvAAAAEIiICA=="), (c) => c.charCodeAt(0));
    expect(imageDimensions(webp)).toEqual([1, 1]);
    expect(() => imageDimensions(webp.subarray(0, 24))).toThrow("malformed");
  });
});

it("bounds and validates chunked upload metadata before it can be signed", async () => {
  const { uploadAttachment } = await import("./attachment");
  const cancel = vi.fn();
  const body = new ReadableStream({ start(c) { c.enqueue(new Uint8Array(16 * 1024 + 1)); }, cancel });
  await expect(uploadAttachment(new Response(body))).rejects.toThrow("too large"); expect(cancel).toHaveBeenCalledOnce();
  await expect(uploadAttachment(new Response(JSON.stringify({ name: "forged" })))).rejects.toThrow("Invalid upload metadata");
  const bytes = new TextEncoder().encode("hello"), att = { id: "file", name: "notes.txt", mime: "text/plain", size: 5, url: "/files/file/notes.txt", sha256: await sha256(bytes) };
  expect(await uploadAttachment(new Response(JSON.stringify(att)))).toEqual(att);
});
it("derives the signed filename and MIME from the user's selected bytes", async () => {
  const { localAttachmentMetadata } = await import("./attachment");
  expect(localAttachmentMetadata("path/.\0 photo.png ", "text/html", Uint8Array.from([137, 80, 78, 71, 13, 10, 26, 10]))).toEqual({ name: " photo.png", mime: "image/png" });
  expect(localAttachmentMetadata("...", "application/octet-stream", new Uint8Array())).toEqual({ name: "file", mime: "application/octet-stream" });
  expect(localAttachmentMetadata(".env", "text/plain", new TextEncoder().encode("OggSxxnot-theora"))).toEqual({ name: "env", mime: "audio/ogg" });
  expect(localAttachmentMetadata("voice.ogg", "text/plain", Uint8Array.from([79, 103, 103, 83, 128, 116, 104, 101, 111, 114, 97]))).toEqual({ name: "voice.ogg", mime: "video/ogg" });
});
