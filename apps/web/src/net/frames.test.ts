import { readFileSync } from "node:fs";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { parseFrame } from "./frames";
import { responseFor } from "./schema";
import { challenge, welcome } from "./testing";

beforeEach(() => { vi.spyOn(console, "warn").mockImplementation(() => {}); });
const message = { id: 1, target: { channel: 1 }, author: 1, author_uid: "uid", author_name: "Name", text: "hello", sent_at: 1 };
const frame = (d: unknown) => JSON.stringify({ ev: "chat.message", d });

describe("bounded server frames", () => {
  it("round-trips the server's serialized original-size recovery plans", () => {
    const frames: unknown[] = JSON.parse(readFileSync(new URL("../../../../crates/gwar-server/tests/screen-recovery-plans.json", import.meta.url), "utf8")) as unknown[];
    for (const frame of frames) {
      expect(parseFrame(JSON.stringify(frame))).toEqual({ kind: "event", event: frame });
    }
    for (const width of [0, -1, 1.5, 589825, Number.MAX_SAFE_INTEGER]) {
      const onInvalid = vi.fn();
      expect(parseFrame(JSON.stringify({ ev: "screen.recover", d: { share: "A".repeat(22), codec: "h264", width, height: 1440, fps: 60 } }), onInvalid)).toBeNull();
      expect(onInvalid).toHaveBeenCalledOnce();
    }
    for (const envelope of [
      { ev: "screen.recover", d: {}, extra: true },
      { ev: "screen.recover", d: {}, re: 1 },
      { ev: "screen.recover" },
    ]) {
      const onInvalid = vi.fn();
      expect(parseFrame(JSON.stringify(envelope), onInvalid)).toBeNull();
      expect(onInvalid).toHaveBeenCalledOnce();
    }
  });

  it("accepts typed events and responses and checks the expected operation", () => {
    expect(parseFrame(frame(message))?.kind).toBe("event");
    expect(parseFrame('{"re":1,"ok":{}}')?.kind).toBe("ok");
    expect(parseFrame('{"re":1,"err":{"code":"forbidden","message":"no"}}')?.kind).toBe("err");
    expect(responseFor("hello", {})).toBe(false);
    expect(responseFor("ping", {})).toBe(true);
    expect(responseFor("chat.send", message)).toBe(true);
    expect(responseFor("chat.history", { messages: [message] })).toBe(true);
  });
  it("bounds the optional screen capability reply without changing legacy welcome", () => {
    expect(responseFor("screen.capabilities", { screen_h264: true })).toBe(true);
    expect(responseFor("screen.capabilities", { screen_h264: false })).toBe(true);
    for (const reply of [{}, { screen_h264: "true" }, { screen_h264: true, extra: true }]) expect(responseFor("screen.capabilities", reply)).toBe(false);
    expect(responseFor("join", welcome())).toBe(true);
    expect(responseFor("join", { ...welcome(), screen_h264: true })).toBe(false);
  });
  it("parses legacy and negotiated shares while bounding every spec field", () => {
    expect(responseFor("screen.spec.capabilities", { screen_spec: true })).toBe(true);
    expect(responseFor("screen.spec.capabilities", { screen_spec: true, extra: 1 })).toBe(false);
    // Old clients' capability shape is unchanged.
    expect(responseFor("screen.capabilities", { screen_h264: true, screen_spec: true })).toBe(false);
    expect(responseFor("screen.av1.capabilities", { screen_av1: true })).toBe(true);
    for (const reply of [{}, { screen_av1: 1 }, { screen_av1: true, screen_h264: true }]) expect(responseFor("screen.av1.capabilities", reply)).toBe(false);
    const share = { share: "A".repeat(22), client: 1, channel: 1, preset: "game", surface: "screen" };
    const frame = (d: unknown) => JSON.stringify({ ev: "screen.started", d });
    expect(parseFrame(frame(share))?.kind).toBe("event");
    expect(parseFrame(frame({ ...share, spec: { mode: "smooth", height: 1440, fps: 60 } }))?.kind).toBe("event");
    for (const spec of [{ mode: "auto", height: 1080, fps: 30 }, { mode: "sharp", height: 2160, fps: 60 }, { mode: "smooth", height: 720, fps: 24 }, { mode: "sharp", height: 1080, fps: 15, bitrate: 1 }]) expect(parseFrame(frame({ ...share, spec }))).toBeNull();
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

describe("gwar/4 runtime schemas", () => {
  it("accepts the owner and the managers in the welcome and their events (6b)", () => {
    const owner = { uid: "A".repeat(27), generation: 1, seq: 0, head: "A".repeat(42) + "E" };
    const managers = { anchor: "QQ", list: "QQ", pending: ["C".repeat(27)], stale: [] };
    expect(responseFor("join", { ...welcome(), owner, managers })).toBe(true);
    expect(responseFor("join", { ...welcome(), owner: null, managers: null })).toBe(true);
    expect(responseFor("join", { ...welcome(), owner: { ...owner, generation: 0 } })).toBe(false);
    expect(parseFrame(JSON.stringify({ ev: "owner.updated", d: owner }))?.kind).toBe("event");
    expect(parseFrame(JSON.stringify({ ev: "managers.updated", d: managers }))?.kind).toBe("event");
    expect(responseFor("owner.chain", { genesis: "g", links: [{ statement: "s", signature: "A".repeat(86) }], next: null })).toBe(true);
    expect(responseFor("owner.transfer", {})).toBe(true);
  });
  it("bounds screen shares in the welcome, their events and replies", () => {
    const share = { share: "A".repeat(22), client: 3, channel: 1, preset: "text", surface: "window" };
    const event = (ev: string, d: unknown) => parseFrame(JSON.stringify({ ev, d }))?.kind;
    expect(responseFor("join", { ...welcome(), shares: [share] })).toBe(true);
    expect(responseFor("join", { ...welcome(), shares: [{ ...share, preset: "4k" }] })).toBe(false);
    const sharer = { id: 3, uid: "uid3", nickname: "Ann", channel: 1, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null };
    expect(responseFor("join", { ...welcome(), clients: [{ ...sharer, sharing: true }] })).toBe(true);
    expect(responseFor("join", { ...welcome(), clients: [{ ...sharer, sharing: "yes" }] })).toBe(false);
    expect(event("screen.started", share)).toBe("event");
    expect(event("screen.started", { ...share, share: "short" })).toBeUndefined();
    expect(event("screen.stopped", { share: share.share, reason: "moderator", by: "mod" })).toBe("event");
    expect(event("screen.stopped", { share: share.share, reason: "access" })).toBe("event");
    expect(event("screen.stopped", { share: share.share, reason: "codec" })).toBe("event");
    expect(event("screen.stopped", { share: share.share, reason: "bored" })).toBeUndefined();
    expect(event("screen.viewers", { share: share.share, members: 2, guests: 0 })).toBe("event");
    expect(event("screen.link", { share: share.share, active: false })).toBe("event");
    expect(responseFor("screen.start", { share: share.share })).toBe(true);
    expect(responseFor("screen.start", {})).toBe(false);
    expect(responseFor("screen.watch", { slot: 3 })).toBe(true);
    expect(responseFor("screen.watch", { slot: 4 })).toBe(false);
    expect(responseFor("screen.watch", {})).toBe(false);
    expect(responseFor("screen.publish.offer", { sdp: "v=0" })).toBe(true);
    expect(responseFor("screen.watch.offer", { sdp: "x".repeat(65537) })).toBe(false);
    expect(event("screen.slot", { run: 1, slot: 0, share: share.share })).toBe("event");
    expect(event("screen.slot", { run: 2, slot: 3, share: null })).toBe("event");
    expect(event("screen.slot", { run: 0, slot: 0, share: null })).toBeUndefined();
    expect(event("screen.slot", { run: 1, slot: 4, share: null })).toBeUndefined();
    expect(event("screen.slot", { run: 1, slot: 0 })).toBeUndefined();
    expect(parseFrame('{"re":1,"err":{"code":"busy","message":"full"}}')?.kind).toBe("err");
    expect(event("screen.paused", { share: share.share, reason: "bandwidth" })).toBe("event");
    expect(event("screen.paused", { share: share.share, reason: "budget" })).toBe("event");
    expect(event("screen.paused", { share: share.share, reason: "tired" })).toBeUndefined();
    expect(event("screen.resumed", { share: share.share })).toBe("event");
    expect(event("screen.resumed", {})).toBeUndefined();
  });
  it("bounds the egress report, its warnings and the screen limits", () => {
    const event = (ev: string, d: unknown) => parseFrame(JSON.stringify({ ev, d }))?.kind;
    const limits = { shares_per_channel: 4, shares_per_server: 32, viewers_per_share: 25, share_cap_kbps: 8000, budget_mbps: 300, monthly_egress_tb: 10, monthly_egress_stop: false };
    const egress = { month: "2026-10", scope: "host", incomplete: false, total: 123, screen_members: 4, screen_guests: 0, limits };
    expect(responseFor("server.egress", egress)).toBe(true);
    expect(responseFor("server.egress", { ...egress, scope: "unavailable", total: null })).toBe(true);
    expect(responseFor("server.egress", { ...egress, incomplete: "false" })).toBe(false);
    expect(responseFor("server.egress", { ...egress, month: "October" })).toBe(false);
    expect(responseFor("server.egress", { ...egress, scope: "cloud" })).toBe(false);
    expect(responseFor("server.egress", { ...egress, limits: { ...limits, budget_mbps: 0 } })).toBe(false);
    expect(responseFor("server.egress", { ...egress, limits: { ...limits, extra: 1 } })).toBe(false);
    expect(event("server.egress_warning", { month: "2026-10", percent: 80, total: 8e12, limit: 1e13 })).toBe("event");
    expect(event("server.egress_warning", { month: "2026-10", percent: 180, total: 8e12, limit: 1e13 })).toBeUndefined();
  });
  it("accepts TeamSpeak group badges in the server info and a client's channel group", () => {
    const w = welcome();
    const badge = { kind: "server", id: 6, name: "Server Admin", sort: 0 };
    const client = { id: 2, uid: "ts:QQ==", nickname: "bob", channel: 1, groups: [6], platform: "ts3", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null, channel_group: 5 };
    expect(responseFor("join", { ...w, server: { ...w.server, badges: [badge] }, clients: [client] })).toBe(true);
    expect(responseFor("join", { ...w, server: { ...w.server, badges: [{ ...badge, kind: "role" }] } })).toBe(false);
    expect(responseFor("join", { ...w, server: { ...w.server, badges: [{ ...badge, name: "x".repeat(300) }] } })).toBe(false);
    expect(parseFrame(JSON.stringify({ ev: "server.updated", d: { ...w.server, badges: [badge] } }))?.kind).toBe("event");
  });
  it("accepts TeamSpeak icon and avatar tokens, never URLs or other text", () => {
    const w = welcome();
    const avatar = `a${"0123456789abcdef".repeat(2)}01234567`;
    const badge = { kind: "server", id: 6, name: "Iconic", sort: 0, icon: "i412469435" };
    const client = { id: 2, uid: "ts:QQ==", nickname: "bob", channel: 1, groups: [6], platform: "ts3", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null, avatar };
    const channel = { id: 1, parent: null, name: "Lobby", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, icon: "i1099912907" };
    const server = { ...w.server, icon: "i1001", badges: [badge, { ...badge, id: 7, icon: null }] };
    expect(responseFor("join", { ...w, server, clients: [client], channels: [channel] })).toBe(true);
    for (const bad of ["https://evil.test/a.png", "blob:https://x/1", "data:image/png;base64,AAAA", "javascript:alert(1)", "i", "i12345678901", "I1001", `a${"0".repeat(39)}`, `A${"0".repeat(40)}`, `a${"g".repeat(40)}`, 7]) {
      expect(responseFor("join", { ...w, server: { ...w.server, icon: bad } })).toBe(false);
      expect(responseFor("join", { ...w, server: { ...w.server, badges: [{ ...badge, icon: bad }] } })).toBe(false);
      expect(parseFrame(JSON.stringify({ ev: "channel.updated", d: { ...channel, icon: bad } }))).toBeNull();
      expect(parseFrame(JSON.stringify({ ev: "client.updated", d: { ...client, avatar: bad } }))).toBeNull();
    }
    expect(parseFrame(JSON.stringify({ ev: "client.joined", d: client }))?.kind).toBe("event");
    expect(parseFrame(JSON.stringify({ ev: "channel.created", d: channel }))?.kind).toBe("event");
  });
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
