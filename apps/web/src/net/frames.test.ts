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
  it("distinguishes sealed history from plaintext history, including empty pages", () => {
    const sealedMessage = {
      id: 1, conversation: `dm:${"A".repeat(27)}:${"B".repeat(27)}`,
      group: "A".repeat(43), epoch: 0, author_uid: "A".repeat(27), author_name: "Name",
      message_id: "A".repeat(22), revision: 0, ciphertext: "AA", sent_at: 1, deleted: false,
    };
    for (const sealed of [[], [sealedMessage]]) {
      const ok = { sealed };
      expect(parseFrame(JSON.stringify({ re: 1, ok }))).toEqual({ kind: "ok", re: 1, ok });
      expect(responseFor("chat.sealed.history", ok)).toBe(true);
      expect(responseFor("chat.history", ok)).toBe(false);
      expect(responseFor("chat.sealed.history", { messages: sealed })).toBe(false);
    }
    for (const messages of [[], [message]]) {
      expect(responseFor("chat.history", { messages })).toBe(true);
      expect(responseFor("chat.sealed.history", { messages })).toBe(false);
    }
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
      expect(parseFrame(`{"ev":"voice.talking","d":{"call":"AAAAAAAAAAAAAAAAAAAAAA","participation":"AQEBAQEBAQEBAQEBAQEBAQ","run":1,"client":1,"talking":true,"${key}":{}}}`)).toBeNull();
      expect(parseFrame(frame({ ...message, target: JSON.parse(`{"channel":1,"${key}":{}}`) }))).toBeNull();
    }
    for (const uid of ["__proto__", "constructor", "toString", "prototype"]) expect(parseFrame(frame({ ...message, author_uid: uid }))).toBeNull();
    expect(({} as { polluted?: boolean }).polluted).toBeUndefined();
  });
  it("bounds all envelopes and voice slots", () => {
    expect(parseFrame('{"ev":"voice.slot","d":{"call":"AAAAAAAAAAAAAAAAAAAAAA","participation":"AQEBAQEBAQEBAQEBAQEBAQ","run":1,"slot":8,"client":1}}')).toBeNull();
    expect(parseFrame('{"ev":"voice.slot","d":{"call":"AAAAAAAAAAAAAAAAAAAAAA","participation":"AQEBAQEBAQEBAQEBAQEBAQ","run":1,"slot":7,"client":null}}')?.kind).toBe("event");
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


describe("voice wire bindings", () => {
  const call = "AAAAAAAAAAAAAAAAAAAAAA";
  const group = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
  it("requires the complete offer answer and exact, safe binding integers", () => {
    const answer = { call, participation: call, run: 1, sdp: "answer" };
    expect(responseFor("voice.offer", answer)).toBe(true);
    for (const run of [0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
      expect(responseFor("voice.offer", { ...answer, run })).toBe(false);
    }
    expect(responseFor("voice.offer", { ...answer, sdp: "x".repeat(65537) })).toBe(false);
    expect(responseFor("voice.offer", { sdp: "answer" })).toBe(false);
    expect(responseFor("voice.offer", { ...answer, call: `${call}=` })).toBe(false);
  });
  it("requires nullable voice fields in server states and roster entries", () => {
    const voice = { channel: 1, call, participation: call, encrypted: false, channel_group: null, group: null, epoch: null };
    expect(responseFor("voice.state", { voice })).toBe(true);
    for (const field of ["channel_group", "group"]) {
      const missing: Record<string, unknown> = { ...voice };
      delete missing[field];
      expect(responseFor("voice.state", { voice: missing })).toBe(false);
      expect(parseFrame(JSON.stringify({ ev: "voice.state", d: { voice: missing } }))).toBeNull();
    }
    const client = { id: 1, uid: "user", nickname: "User", channel: null, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: false, voice_binding: null };
    expect(parseFrame(JSON.stringify({ ev: "client.updated", d: client }))?.kind).toBe("event");
    const missing: Record<string, unknown> = { ...client };
    delete missing.voice_binding;
    expect(parseFrame(JSON.stringify({ ev: "client.updated", d: missing }))).toBeNull();
  });

  it("validates voice state modes and nullable group/epoch as a pair", () => {
    const voice = { channel: 1, call, participation: call, encrypted: true, channel_group: group, group: null, epoch: null };
    expect(responseFor("voice.state", { voice })).toBe(true);
    expect(responseFor("voice.state", { voice: null })).toBe(true);
    for (const invalid of [{ ...voice, encrypted: false }, { ...voice, channel_group: null }, { ...voice, group }, { ...voice, group, epoch: Number.MAX_SAFE_INTEGER + 1 }]) {
      expect(responseFor("voice.state", { voice: invalid })).toBe(false);
    }
    const event = { ev: "voice.epoch.ack", d: { call, group, epoch: 1, device: group } };
    expect(parseFrame(JSON.stringify(event))?.kind).toBe("event");
    expect(parseFrame(JSON.stringify({ ...event, d: { ...event.d, epoch: 1.5 } }))).toBeNull();
    expect(parseFrame(JSON.stringify({ ...event, d: { ...event.d, session: 1 } }))).toBeNull();
  });
});
