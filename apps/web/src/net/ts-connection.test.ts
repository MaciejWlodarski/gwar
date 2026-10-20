import { describe, expect, it } from "vitest";
import type { TauriBridge } from "../platform";
import { parseTeamSpeakAddress } from "./address";
import { RequestError } from "./connection";
import { classifyTsError, TsConnection } from "./ts-connection";

describe("parseTeamSpeakAddress", () => {
  it("defaults the port to 9987", () => {
    expect(parseTeamSpeakAddress("ts.example.com")).toEqual({ ok: true, value: { address: "ts.example.com:9987", host: "ts.example.com", label: "ts.example.com" } });
  });
  it("keeps an explicit port and strips ts3server://", () => {
    const r = parseTeamSpeakAddress("ts3server://10.0.0.2:10011/?port=1");
    expect(r.ok && r.value.address).toBe("10.0.0.2:10011");
  });
  it("brackets IPv6", () => {
    const r = parseTeamSpeakAddress("::1");
    expect(r.ok && r.value).toMatchObject({ address: "[::1]:9987", host: "::1" });
    const r2 = parseTeamSpeakAddress("[fe80::1]:9988");
    expect(r2.ok && r2.value.address).toBe("[fe80::1]:9988");
  });
  it("rejects garbage", () => {
    expect(parseTeamSpeakAddress("  ")).toEqual({ ok: false, error: "empty" });
    expect(parseTeamSpeakAddress("a b")).toEqual({ ok: false, error: "invalid" });
    expect(parseTeamSpeakAddress("host:99999")).toEqual({ ok: false, error: "invalid" });
  });
});

describe("classifyTsError", () => {
  it("recognises password problems", () => {
    expect(classifyTsError("server password incorrect", true).kind).toBe("wrong_password");
    expect(classifyTsError("password required", false).kind).toBe("password_required");
  });
  it("falls back to unreachable", () => {
    expect(classifyTsError("no route", false).kind).toBe("unreachable");
  });
});

function fakeBridge(reply?: (op: string) => unknown) {
  const listeners = new Map<string, (p: never) => void>();
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const bridge: TauriBridge = {
    invoke: (async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      if (command === "ts_connect") {
        // An event races the welcome reply.
        listeners.get("ts://event")?.({ session: args?.session, frame: JSON.stringify({ ev: "voice.talking", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, client: 1, talking: true } }) } as never);
        return { session: 1, uid: "uid", server: { name: "TS", welcome: "", version: "1", default_channel: 1, max_clients: 64, upload_limit: 0, file_access: "link", teamspeak: true, server_chat: { encrypted: false, history: false } }, permissions: [], groups: [], channels: [], clients: [], members: [], unread: [], conversations: [], ice_servers: [], file_access: "" };
      }
      if (command === "ts_request") {
        if (reply) return reply(String(args?.op));
        throw { code: "forbidden", message: "no" };
      }
      return undefined;
    }) as TauriBridge["invoke"],
    listen: async (name, handler) => {
      listeners.set(name, handler as (p: never) => void);
      return () => listeners.delete(name);
    },
  };
  return { bridge, calls, listeners };
}

describe("TsConnection", () => {
  it("delivers the welcome before events that raced it, and maps request errors", async () => {
    const { bridge, calls } = fakeBridge();
    const conn = new TsConnection({ address: "h:9987", nickname: "n", identity: "uid-1", bridge: async () => bridge });
    const seen: string[] = [];
    conn.onWelcome(() => seen.push("welcome"));
    conn.onEvent((e) => seen.push(e.ev));
    await conn.connect();
    expect(seen).toEqual(["welcome", "voice.talking"]);
    expect(conn.isOnline).toBe(true);
    await expect(conn.request("ping", {})).rejects.toBeInstanceOf(RequestError);
    expect(calls.find((c) => c.command === "ts_connect")?.args).toMatchObject({ address: "h:9987", nickname: "n", password: null, identity: "uid-1" });
    conn.close();
    expect(calls.at(-1)?.command).toBe("ts_disconnect");
  });
});

describe("TsConnection replies", () => {
  // What `gwar_client::teamspeak` answers for done commands.
  const replies: Record<string, unknown> = {
    "chat.send": { id: 2147483649, target: { channel: 4 }, author: 3, author_uid: "ts:AAAAAAAAAAAAAAAAAAAAAAAAAAA=", author_name: "me", text: "hi", sent_at: 1 },
    "channel.create": { id: 9, parent: 4, name: "New", topic: "", position: 0, has_password: false, max_clients: 5, encrypted: false, history: false, private: false, synced: false },
    "token.redeem": { groups: [6] },
    "channel.join": {},
  };

  it("accepts the native client's answers in Gwar shapes", async () => {
    const { bridge } = fakeBridge((op) => replies[op]);
    const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
    await conn.connect();
    await expect(conn.request("chat.send", { target: { channel: 4 }, text: "hi" } as never)).resolves.toMatchObject({ text: "hi" });
    await expect(conn.request("channel.create", { name: "New", parent: 4 } as never)).resolves.toMatchObject({ id: 9 });
    await expect(conn.request("token.redeem", { token: "t" })).resolves.toEqual({ groups: [6] });
    await expect(conn.request("channel.join", { channel: 4, password: null })).resolves.toEqual({});
  });

  it("refuses an empty answer where a message is due", async () => {
    // An empty `chat.send` answer once failed every TeamSpeak message after it had gone out.
    const { bridge } = fakeBridge(() => ({}));
    const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
    await conn.connect();
    await expect(conn.request("chat.send", { target: { channel: 4 }, text: "hi" } as never)).rejects.toBeInstanceOf(RequestError);
  });
});
