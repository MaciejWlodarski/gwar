import { attachment as attachmentSchema } from "./schema";
import type { Attachment } from "../proto/Attachment";
import { fetchFileBytes, type FileOptions } from "../platform/download";
import { sha256 } from "./message-signature";
import { openFile } from "./sealed-file";
import { isInlineImage } from "../lib/files";

/** Nothing is displayed or saved from a signed attachment until its complete bytes hash correctly. An
 * encrypted one is checked as downloaded, decrypted, and checked again as the file its sender signed. */
export async function verifiedAttachmentBlob(att: Attachment, path: string, fetchBytes = fetchFileBytes, opts: FileOptions = { origin: null, size: att.size, uploadLimit: 30 * 1024 * 1024 }): Promise<Blob> {
  let bytes: Uint8Array<ArrayBuffer>;
  if (att.sealed) {
    const ciphertext = await fetchBytes(path, { ...opts, size: att.sealed.size });
    if (ciphertext.byteLength !== att.sealed.size || await sha256(ciphertext as BufferSource) !== att.sealed.sha256) throw new Error("Attachment hash mismatch");
    bytes = await openFile(ciphertext, att.sealed.key);
  } else bytes = await fetchBytes(path, opts);
  if (bytes.byteLength !== att.size || await sha256(bytes as BufferSource) !== att.sha256) throw new Error("Attachment hash mismatch");
  // The sender's size hints must describe the image it encrypted.
  if (att.sealed && (att.width || att.height)) {
    const [width, height] = imageDimensions(bytes);
    if (width !== att.width || height !== att.height) throw new Error("Attachment dimensions mismatch");
  }
  return new Blob([bytes as BlobPart], { type: att.mime });
}
export async function verifiedAttachmentUrl(att: Attachment, path: string, fetchBytes = fetchFileBytes, opts?: FileOptions): Promise<string> {
  return URL.createObjectURL(await boundedImageBlob(await verifiedAttachmentBlob(att, path, fetchBytes, opts)));
}
export const MAX_IMAGE_SIDE = 4096;
export const MAX_IMAGE_PIXELS = 16_000_000;
export function checkImageDimensions(width: number, height: number): void {
  if (!Number.isSafeInteger(width) || !Number.isSafeInteger(height) || width < 1 || height < 1
      || width > MAX_IMAGE_SIDE || height > MAX_IMAGE_SIDE || width * height > MAX_IMAGE_PIXELS) throw new Error("Image exceeds decoded dimensions limit");
}
/**
 * Read dimensions before decoding to avoid accepting compressed image bombs. The
 * answer must be what every real decoder will allocate, so anything a decoder
 * could size differently is refused: a GIF frame outside its logical screen
 * (Chromium enlarges the image to its first frame), more than one JPEG frame
 * header (WebKit sizes by the last one), a PNG whose first chunk is not IHDR.
 */
export function imageDimensions(bytes: Uint8Array): [number, number] {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const ascii = (at: number, n: number) => new TextDecoder().decode(bytes.subarray(at, at + n));
  if (bytes.length >= 24 && bytes[0] === 137 && ascii(1, 3) === "PNG") {
    if (ascii(12, 4) !== "IHDR") throw new Error("Unsupported or malformed inline image");
    return [v.getUint32(16), v.getUint32(20)];
  }
  if (bytes.length >= 13 && ascii(0, 3) === "GIF") return gifDimensions(bytes, v);
  // Each WebP kind ends its dimensions at its own offset: a tiny lossless image is under 30 bytes.
  if (bytes.length >= 16 && ascii(0, 4) === "RIFF" && ascii(8, 4) === "WEBP") {
    const kind = ascii(12, 4);
    const u24 = (at: number) => bytes[at]! + bytes[at + 1]! * 256 + bytes[at + 2]! * 65536;
    if (kind === "VP8X" && bytes.length >= 30) return [1 + u24(24), 1 + u24(27)];
    if (kind === "VP8 " && bytes.length >= 30 && bytes[23] === 157 && bytes[24] === 1 && bytes[25] === 42) return [v.getUint16(26, true) & 16383, v.getUint16(28, true) & 16383];
    if (kind === "VP8L" && bytes.length >= 25 && bytes[20] === 47) {
      const bits = v.getUint32(21, true); return [(bits & 16383) + 1, ((bits >>> 14) & 16383) + 1];
    }
  }
  if (bytes[0] === 255 && bytes[1] === 216) return jpegDimensions(bytes, v);
  throw new Error("Unsupported or malformed inline image");
}
const malformed = () => new Error("Unsupported or malformed inline image");
/** The logical screen, once every frame is known to lie inside it. */
function gifDimensions(bytes: Uint8Array, v: DataView): [number, number] {
  const width = v.getUint16(6, true), height = v.getUint16(8, true);
  const table = (flags: number) => (flags & 0x80 ? 3 * 2 ** ((flags & 7) + 1) : 0);
  // Data sub-blocks: a length byte, that many bytes, until a zero length.
  const skipBlocks = (at: number) => {
    while (at < bytes.length && bytes[at] !== 0) at += 1 + bytes[at]!;
    if (at >= bytes.length) throw malformed();
    return at + 1;
  };
  let at = 13 + table(bytes[10]!), frames = 0;
  for (;;) {
    if (at >= bytes.length) throw malformed();
    const block = bytes[at]!;
    if (block === 0x3b) break;
    if (block === 0x21) { at = skipBlocks(at + 2); continue; }
    if (block !== 0x2c || at + 10 > bytes.length) throw malformed();
    const left = v.getUint16(at + 1, true), top = v.getUint16(at + 3, true);
    const w = v.getUint16(at + 5, true), h = v.getUint16(at + 7, true);
    if (left + w > width || top + h > height) throw new Error("GIF frame outside its image");
    frames++;
    at = skipBlocks(at + 10 + table(bytes[at + 9]!) + 1);
  }
  if (frames === 0) throw malformed();
  return [width, height];
}
const JPEG_FRAMES = new Set([192, 193, 194, 195, 197, 198, 199, 201, 202, 203, 205, 206, 207]);
/** The one frame header before the first scan; a second one is refused. */
function jpegDimensions(bytes: Uint8Array, v: DataView): [number, number] {
  let at = 2, found: [number, number] | null = null;
  for (;;) {
    if (at + 1 >= bytes.length || bytes[at] !== 255) throw malformed();
    while (bytes[at] === 255 && at < bytes.length) at++;
    const marker = bytes[at++];
    if (marker === undefined || marker === 216 || marker === 217) throw malformed();
    if (marker === 1 || (marker >= 208 && marker <= 215)) continue;
    if (at + 2 > bytes.length) throw malformed();
    const size = v.getUint16(at);
    if (size < 2 || at + size > bytes.length) throw malformed();
    if (marker === 218) break;
    if (JPEG_FRAMES.has(marker)) {
      if (found) throw new Error("JPEG with more than one frame header");
      if (size < 7) throw malformed();
      found = [v.getUint16(at + 5), v.getUint16(at + 3)];
    }
    at += size;
  }
  if (!found) throw malformed();
  return found;
}
/** An encrypted image's size, which only its sender can measure: unsigned display hints,
 * checked against the decrypted image before it is shown. Like the server's for a plaintext
 * image they are the header's dimensions, before any EXIF rotation; the box only sizes a
 * placeholder, and the decoded image is fitted into it. */
export function imageHints(mime: string, bytes: Uint8Array): { width?: number; height?: number } {
  if (!isInlineImage(mime)) return {};
  try {
    const [width, height] = imageDimensions(bytes);
    checkImageDimensions(width, height);
    return { width, height };
  } catch { return {}; }
}
/** Convert the bounded decoded first frame to a static PNG, including explicitly loaded unsigned images. */
export async function boundedImageBlob(blob: Blob, beforeDecode?: (width: number, height: number) => Promise<void>): Promise<Blob> {
  if (!blob.type.startsWith("image/")) return blob;
  const [width, height] = imageDimensions(new Uint8Array(await blob.arrayBuffer()));
  checkImageDimensions(width, height);
  await beforeDecode?.(width, height);
  const bitmap = await createImageBitmap(blob);
  try {
    checkImageDimensions(bitmap.width, bitmap.height);
    if (bitmap.width * bitmap.height > width * height) throw new Error("Image decodes larger than its header");
    const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
    const context = canvas.getContext("2d");
    if (!context) throw new Error("Image decoder unavailable");
    context.drawImage(bitmap, 0, 0);
    return await canvas.convertToBlob({ type: "image/png" });
  } finally { bitmap.close(); }
}

/** New upload metadata is hostile HTTP input, not an instruction for what to sign. */
export async function uploadAttachment(response: Response): Promise<Attachment> {
  const cap = 16 * 1024;
  const reader = response.body?.getReader();
  if (!reader) throw new Error("Missing upload metadata");
  try {
    if (Number(response.headers.get("Content-Length")) > cap) throw new Error("Upload metadata too large");
    const decoder = new TextDecoder(); let text = "", size = 0;
    while (true) {
      const chunk = await reader.read(); if (chunk.done) break;
      size += chunk.value.length;
      if (size > cap) throw new Error("Upload metadata too large");
      text += decoder.decode(chunk.value, { stream: true });
    }
    const value: unknown = JSON.parse(text + decoder.decode());
    if (!attachmentSchema(value)) throw new Error("Invalid upload metadata");
    return value;
  } catch (e) { await reader.cancel().catch(() => {}); throw e; }
  finally { reader.releaseLock(); }
}

/** Mirrors honest-server filename cleanup and byte sniffing, so a server cannot choose signed metadata. */
export function localAttachmentMetadata(name: string, mime: string, bytes: Uint8Array): { name: string; mime: string } {
  const base = name.split(/[\\/]/).at(-1) ?? "";
  const clean = [...base].filter((c) => !/\p{Cc}/u.test(c)).slice(0, 120).join("")
    .replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "").replace(/^\.+/, "") || "file";
  const header = bytes.subarray(0, 64 * 1024);
  const at = (offset: number, magic: string | number[]) => [...magic].every((c, i) => header[offset + i] === (typeof c === "string" ? c.charCodeAt(0) : c));
  let detected: string | undefined;
  if (at(0, [137, 80, 78, 71, 13, 10, 26, 10])) detected = "image/png";
  else if (at(0, [255, 216, 255])) detected = "image/jpeg";
  else if (at(0, "GIF87a") || at(0, "GIF89a")) detected = "image/gif";
  else if (at(0, "RIFF") && at(8, "WEBP")) detected = "image/webp";
  else if (at(4, "ftyp")) detected = at(8, "M4A ") || at(8, "M4B ") ? "audio/mp4" : "video/mp4";
  else if (at(0, [26, 69, 223, 163])) detected = "video/webm";
  else if (at(0, "OggS")) detected = header.some((b, i) => b === 128 && at(i + 1, "theora")) ? "video/ogg" : "audio/ogg";
  else if (at(0, "RIFF") && at(8, "WAVE")) detected = "audio/wav";
  else if (at(0, "fLaC")) detected = "audio/flac";
  else if (at(0, "ID3") || (header.length > 1 && header[0] === 255 && (header[1]! & 224) === 224)) detected = "audio/mpeg";
  return { name: clean, mime: detected ?? [...mime].filter((c) => /^[!-~]$/.test(c)).slice(0, 100).join("") };
}
