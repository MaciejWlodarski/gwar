import { beforeEach, describe, expect, it, vi } from "vitest";
import { parseFrame } from "./frames";
import { responseFor } from "./schema";

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
