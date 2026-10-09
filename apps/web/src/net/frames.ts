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
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const o = value as Record<string, unknown>;
  if (typeof o.re === "number") {
    if ("ok" in o) return { kind: "ok", re: o.re, ok: o.ok };
    if (typeof o.err === "object" && o.err !== null) return { kind: "err", re: o.re, err: o.err as ErrorBody };
    return null;
  }
  if (typeof o.ev === "string") return { kind: "event", event: o as unknown as Event };
  return null;
}
