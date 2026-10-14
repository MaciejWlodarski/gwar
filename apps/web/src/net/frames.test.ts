import { beforeEach, describe, expect, it, vi } from "vitest";
import { parseFrame } from "./frames";
import { responseFor } from "./schema";
import { challenge, welcome } from "./testing";

beforeEach(() => { vi.spyOn(console, "warn").mockImplementation(() => {}); });
const message = { id: 1, target: { channel: 1 }, author: 1, author_uid: "uid", author_name: "Name", text: "hello", sent_at: 1 };
const frame = (d: unknown) => JSON.stringify({ ev: "chat.message", d });

describe("bounded server frames", () => {
  it("accepts typed events and responses and checks the expected operation", () => {
    expect(parseFrame(frame(message))?.kind).toBe("event");
    expect(parseFrame('{"re":1,"ok":{}}')?.kind).toBe("ok");
    expect(parseFrame('{"re":1,"err":{"code":"forbidden","message":"no"}}')?.kind).toBe("err");
    expect(responseFor("hello", {})).toBe(false);
    expect(responseFor("ping", {})).toBe(true);
    expect(responseFor("chat.send", message)).toBe(true);
    expect(responseFor("chat.history", { messages: [message] })).toBe(true);
  });
  it("drops huge strings, arrays, nesting, invalid types and unsafe integers without throwing", () => {
    let deep: unknown = {};
    for (let i = 0; i < 1000; i++) deep = { child: deep };
    const invalid = [
      { ...message, text: "x".repeat(4097) }, { ...message, text: deep }, { ...message, target: deep },
      { ...message, text: 1 }, { ...message, sent_at: -1 }, { ...message, id: Number.MAX_SAFE_INTEGER + 1 },
      { ...message, mentions: Array(129).fill("uid") }, { ...message, attachments: Array(11).fill({}) },
      { ...message, attachments: [{ id: "a", name: "x", mime: "image/png", size: 1, url: "http://localhost/x" }] },
      null, [], "message",
    ];
    for (const d of invalid) expect(parseFrame(frame(d))).toBeNull();
    for (const raw of ["{", "null", "[]", '{"ev":"unknown","d":{}}', '{"re":1.5,"ok":{}}', '{"re":1,"ok":{},"err":{}}', "x".repeat(2 * 1024 * 1024 + 1)]) {
      expect(() => parseFrame(raw)).not.toThrow();
      expect(parseFrame(raw)).toBeNull();
    }
    expect(console.warn).toHaveBeenCalled();
  });
  it("rejects prototype-pollution keys at every payload level", () => {
    for (const key of ["__proto__", "constructor", "prototype"]) {
      expect(parseFrame(`{"re":1,"ok":{"${key}":{}}}`)).toBeNull();
      expect(parseFrame(`{"ev":"voice.talking","d":{"client":1,"talking":true,"${key}":{}}}`)).toBeNull();
      expect(parseFrame(frame({ ...message, target: JSON.parse(`{"channel":1,"${key}":{}}`) }))).toBeNull();
    }
    for (const uid of ["__proto__", "constructor", "toString", "prototype"]) expect(parseFrame(frame({ ...message, author_uid: uid }))).toBeNull();
    expect(({} as { polluted?: boolean }).polluted).toBeUndefined();
  });
  it("bounds all envelopes and voice slots", () => {
    expect(parseFrame('{"ev":"voice.slot","d":{"slot":8,"client":1}}')).toBeNull();
    expect(parseFrame('{"ev":"voice.slot","d":{"slot":7,"client":null}}')?.kind).toBe("event");
    expect(parseFrame('{"ev":"disconnected","d":{"reason":{"kind":"kicked","by":"x","reason":null}}}')?.kind).toBe("event");
    expect(responseFor("ban.list", { bans: Array(4097).fill({}) })).toBe(false);
    expect(responseFor("file.upload", { file: "a", upload_url: "/files/a/b" })).toBe(false);
    expect(responseFor("voice.offer", { sdp: "x".repeat(65537) })).toBe(false);
  });
  it("drops deterministic fuzz mutations", () => {
    const values = [null, {}, [], true, false, -1, 1.2, 1e100, "x".repeat(5000)];
    for (const key of Object.keys(message)) {
      for (const value of values) {
        const raw = frame({ ...message, [key]: value });
        expect(() => parseFrame(raw)).not.toThrow();
        expect(parseFrame(raw)).toBeNull();
      }
    }
  });
});

describe("vc/4 runtime schemas", () => {
  it("validates challenge keys and nonces with exact fields and canonical encodings", () => {
    const valid = challenge();
    expect(parseFrame(JSON.stringify(valid))?.kind).toBe("event");
    for (const field of ["server_pk", "server_nonce"] as const) {
      for (const value of [null, 32, "", "A".repeat(42), "A".repeat(44), "B".repeat(43), `${valid.d[field]}=`, `${valid.d[field]}\n`]) {
        expect(parseFrame(JSON.stringify({ ...valid, d: { ...valid.d, [field]: value } }))).toBeNull();
      }
      const missing = { ...valid.d } as Partial<typeof valid.d>;
      delete missing[field];
      expect(parseFrame(JSON.stringify({ ...valid, d: missing }))).toBeNull();
    }
    expect(parseFrame(JSON.stringify({ ...valid, d: { ...valid.d, nonce: "legacy" } }))).toBeNull();
    expect(parseFrame(JSON.stringify({ ...valid, d: { ...valid.d, extra: true } }))).toBeNull();
  });
  it("accepts exactly server_proof for hello and Welcome only for join", () => {
    const proof = { server_proof: "A".repeat(86) };
    expect(responseFor("hello", proof)).toBe(true);
    expect(parseFrame(JSON.stringify({ re: 1, ok: proof }))?.kind).toBe("ok");
    expect(responseFor("hello", welcome())).toBe(false);
    expect(responseFor("join", welcome())).toBe(true);
    expect(responseFor("join", proof)).toBe(false);
    // The download token becomes an HTTP header value: URL-safe characters only.
    expect(responseFor("join", welcome({ file_access: "" }))).toBe(true);
    for (const file_access of ["a\r\nX-Evil: 1", "a b", "x".repeat(129)]) expect(responseFor("join", welcome({ file_access }))).toBe(false);
    expect(responseFor("join", welcome({ server: { ...welcome().server, file_access: "members" } }))).toBe(true);
    expect(responseFor("join", welcome({ server: { ...welcome().server, file_access: "anyone" as never } }))).toBe(false);
    for (const value of [null, [], { ...proof, extra: true }, { server_proof: 1 }, { server_proof: "B".repeat(86) }, { server_proof: `${proof.server_proof}=` }]) {
      expect(responseFor("hello", value)).toBe(false);
      expect(parseFrame(JSON.stringify({ re: 1, ok: value }))).toBeNull();
    }
  });
});
