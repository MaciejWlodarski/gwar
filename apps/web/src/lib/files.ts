/** Helpers for attachments: sizes, kinds and the client-side upload checks. */

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = -1;
  do {
    value /= 1024;
    unit++;
  } while (value >= 1024 && unit < units.length - 1);
  return `${value >= 100 || Number.isInteger(value) ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/** Raster images the server serves inline (never SVG). */
export function isInlineImage(mime: string): boolean {
  return /^image\/(png|jpeg|gif|webp)$/i.test(mime);
}

export type FileKind = "image" | "audio" | "video" | "archive" | "text" | "other";

export function fileKind(mime: string, name: string): FileKind {
  if (isInlineImage(mime) || mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  if (mime.startsWith("video/")) return "video";
  if (/\.(zip|7z|rar|tar|gz|bz2|xz)$/i.test(name) || /zip|compressed|tar/.test(mime)) return "archive";
  if (mime.startsWith("text/") || /\.(txt|md|log|json|csv|pdf|docx?|xlsx?)$/i.test(name) || /pdf|json|xml/.test(mime)) return "text";
  return "other";
}

export type UploadCheck = { ok: true } | { ok: false; reason: "disabled" | "too_large" | "empty" };

/** `limit` is the server's `upload_limit` (0: uploads are off). */
export function checkUpload(size: number, limit: number): UploadCheck {
  if (limit <= 0) return { ok: false, reason: "disabled" };
  if (size <= 0) return { ok: false, reason: "empty" };
  if (size > limit) return { ok: false, reason: "too_large" };
  return { ok: true };
}

export interface Box {
  width: number;
  height: number;
}

/** The display size of an image: its own size scaled down to fit the box, never up. */
export function fitBox(width: number, height: number, maxW = 400, maxH = 300): Box {
  const scale = Math.min(1, maxW / width, maxH / height);
  return { width: Math.max(1, Math.round(width * scale)), height: Math.max(1, Math.round(height * scale)) };
}
