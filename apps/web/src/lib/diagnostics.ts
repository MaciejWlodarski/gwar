/**
 * What a bug report carries besides the person's words: the app's recent warnings and errors (kept
 * in memory only, never stored) and a description of its state. Nothing secret goes in: no keys,
 * tokens, passwords or message contents.
 */

export interface LogEntry { at: number; level: "warn" | "error"; text: string }

const MAX_ENTRIES = 80;
const MAX_TEXT = 600;
const entries: LogEntry[] = [];
let installed = false;

function describe(value: unknown): string {
  if (value instanceof Error) return `${value.name}: ${value.message}`;
  if (typeof value === "string") return value;
  try { return JSON.stringify(value); } catch { return String(value); }
}

function push(level: LogEntry["level"], args: unknown[]): void {
  const text = args.map(describe).join(" ").slice(0, MAX_TEXT);
  entries.push({ at: Date.now(), level, text });
  if (entries.length > MAX_ENTRIES) entries.splice(0, entries.length - MAX_ENTRIES);
}

/** Keeps the latest console warnings and errors, and uncaught errors, for a report. */
export function captureLogs(): void {
  if (installed || typeof console === "undefined") return;
  installed = true;
  for (const level of ["warn", "error"] as const) {
    const original = console[level].bind(console);
    console[level] = (...args: unknown[]) => { push(level, args); original(...args); };
  }
  if (typeof window !== "undefined") {
    window.addEventListener("error", (e) => push("error", [e.message || e.error]));
    window.addEventListener("unhandledrejection", (e) => push("error", ["Unhandled rejection:", e.reason]));
  }
}

export function recentLogs(): LogEntry[] {
  return entries.map((e) => ({ ...e }));
}

/** The largest diagnostics Connect accepts; the oldest log lines go first to fit. */
export const MAX_DIAGNOSTICS_BYTES = 60 * 1024;

export function fitDiagnostics(diagnostics: Record<string, unknown> & { logs: LogEntry[] }): Record<string, unknown> {
  const out = { ...diagnostics, logs: [...diagnostics.logs] };
  while (new TextEncoder().encode(JSON.stringify(out)).length > MAX_DIAGNOSTICS_BYTES && out.logs.length) out.logs.shift();
  return out;
}

/** Screenshots Connect takes: PNG, JPEG, WebP or GIF of at most 4 MiB. */
export const MAX_SCREENSHOT_BYTES = 4 * 1024 * 1024;
const TYPES = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/** A screenshot as sent: as it is when it fits, otherwise re-encoded as JPEG (and scaled down). */
export async function fitScreenshot(file: Blob): Promise<Blob> {
  if (!TYPES.includes(file.type)) throw new Error("unsupported image");
  if (file.size <= MAX_SCREENSHOT_BYTES) return file;
  const bitmap = await createImageBitmap(file);
  try {
    for (const [side, quality] of [[3200, 0.85], [2400, 0.8], [1600, 0.75]] as const) {
      const scale = Math.min(1, side / Math.max(bitmap.width, bitmap.height));
      const canvas = new OffscreenCanvas(Math.round(bitmap.width * scale), Math.round(bitmap.height * scale));
      canvas.getContext("2d")!.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
      const blob = await canvas.convertToBlob({ type: "image/jpeg", quality });
      if (blob.size <= MAX_SCREENSHOT_BYTES) return blob;
    }
  } finally { bitmap.close(); }
  throw new Error("image too large");
}
