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
  const bytes = new Uint8Array(24); bytes.set([137, 80, 78, 71]);
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
