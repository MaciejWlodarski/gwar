import { anyResponse, error, events, record } from "./schema";
import type { ErrorBody } from "../proto/ErrorBody";
import type { Event } from "../proto/Event";
import type { Op, RequestData } from "./protocol";

export type ParsedFrame =
  | { kind: "ok"; re: number; ok: unknown }
  | { kind: "err"; re: number; err: ErrorBody }
  | { kind: "event"; event: Event };

/** Serialises a request as `{"id":n,"op":"...","d":{...}}`. */
export function encodeRequest<O extends Op>(id: number, op: O, d: Partial<RequestData<O>> | Record<string, unknown>): string {
  return JSON.stringify({ id, op, d });
}

/** Classifies a server text frame. Returns null for anything malformed. */
export function parseFrame(text: string): ParsedFrame | null {
  const reject = () => { console.warn("Dropped malformed server frame"); return null; };
  // Limit before JSON.parse; nested structures are rejected by exact, non-recursive schemas.
  if (text.length > 2 * 1024 * 1024) return reject();
  let value: unknown;
  try { value = JSON.parse(text); } catch { return reject(); }
  if (!record(value)) return reject();
  const o = value;
  if (Object.hasOwn(o, "re")) {
    if (typeof o.re !== "number" || !Number.isSafeInteger(o.re) || o.re < 0 || o.re > 0xffffffff || Object.keys(o).length !== 2) return reject();
    if (Object.hasOwn(o, "ok") && anyResponse(o.ok)) return { kind: "ok", re: o.re, ok: o.ok };
    if (Object.hasOwn(o, "err") && error(o.err)) return { kind: "err", re: o.re, err: o.err };
    return reject();
  }
  if (typeof o.ev !== "string" || Object.keys(o).length !== 2 || !Object.hasOwn(events, o.ev)) return reject();
  const name = o.ev as keyof typeof events;
  if (!events[name](o.d)) return reject();
  return { kind: "event", event: o as unknown as Event };
}
