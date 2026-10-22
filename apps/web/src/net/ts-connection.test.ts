import { describe, expect, it, vi } from "vitest";
import type { TauriBridge } from "../platform";
import { parseTeamSpeakAddress } from "./address";
import { RequestError } from "./connection";
import { classifyTsError, TsConnection } from "./ts-connection";
import { useTsImages } from "./ts-images";

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
        return { session: 1, uid: "uid", server: { name: "TS", welcome: "", version: "1", default_channel: 1, max_clients: 64, upload_limit: 0, file_access: "link", teamspeak: true, screen_guest_links: true, server_chat: { encrypted: false, history: false } }, permissions: [], groups: [], channels: [], clients: [], members: [], unread: [], conversations: [], ice_servers: [], file_access: "" };
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
    expect(calls.find((c) => c.command === "ts_connect")?.args).toMatchObject({ address: "h:9987", nickname: "n", password: null, identity: "uid-1", channel: null, channelPassword: null });
    conn.close();
    expect(calls.at(-1)?.command).toBe("ts_disconnect");
  });

  it("hands out names and topics without formatting that would reorder them", async () => {
    const { bridge, listeners, calls } = fakeBridge((op) => (op === "channel.create" ? { id: 9, parent: null, name: "N\u202eew", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false } : {}));
    const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
    const seen: unknown[] = [];
    conn.onEvent((e) => seen.push(e.d));
    await conn.connect();
    const mallory = { id: 4, uid: "ts:QQ==", nickname: "Mal\u202elory", channel: 1, groups: [], platform: "ts3", muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null };
    const session = calls.find((c) => c.command === "ts_connect")?.args?.session;
    listeners.get("ts://event")?.({ session, frame: JSON.stringify({ ev: "client.joined", d: mallory }) } as never);
    expect(seen.at(-1)).toMatchObject({ nickname: "Mallory" });
    await expect(conn.request("channel.create", { name: "New", parent: null } as never)).resolves.toMatchObject({ name: "New" });
    conn.close();
  });

  it("asks for a bookmark's channel and its password", async () => {
    const { bridge, calls } = fakeBridge();
    const conn = new TsConnection({ address: "h:9987", nickname: "n", channel: "Lobby/Games", channelPassword: "pw", bridge: async () => bridge });
    await conn.connect();
    expect(calls.find((c) => c.command === "ts_connect")?.args).toMatchObject({ channel: "Lobby/Games", channelPassword: "pw" });
    conn.close();
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

  it("loads the icons and avatars events name through ts_image, and drops them on close", async () => {
    const { bridge, calls, listeners } = fakeBridge();
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
    const invoke = bridge.invoke;
    bridge.invoke = (async (command: string, args?: Record<string, unknown>) =>
      command === "ts_image" ? (calls.push({ command, args }), args?.token === "i1001" ? png.buffer : "<svg/>") : invoke(command, args)) as typeof bridge.invoke;
    vi.spyOn(URL, "createObjectURL").mockImplementation(() => "blob:gwar/1");
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
    await conn.connect();
    const session = calls.find((c) => c.command === "ts_connect")?.args?.session;
    const channel = { id: 1, parent: null, name: "Lobby", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false };
    const send = (icon: string) => listeners.get("ts://event")?.({ session, frame: JSON.stringify({ ev: "channel.updated", d: { ...channel, icon } }) } as never);
    send("i1001");
    send("i1002");
    send("i1001");
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(calls.filter((c) => c.command === "ts_image").map((c) => c.args)).toEqual([{ session, token: "i1001" }, { session, token: "i1002" }]);
    // The SVG answer for i1002 is no image.
    expect(useTsImages.getState().urls).toEqual({ i1001: "blob:gwar/1" });
    conn.close();
    expect(useTsImages.getState().urls).toEqual({});
    expect(revoke).toHaveBeenCalledWith("blob:gwar/1");
    vi.restoreAllMocks();
  });

  it("drops the images when the native session closes, also for good, and lands no late load", async () => {
    for (const terminal of [false, true]) {
      useTsImages.setState({ urls: {} });
      const { bridge, calls, listeners } = fakeBridge();
      const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13]);
      const late: Array<() => void> = [];
      const invoke = bridge.invoke;
      bridge.invoke = (async (command: string, args?: Record<string, unknown>) => {
        if (command !== "ts_image") return invoke(command, args);
        calls.push({ command, args });
        if (args?.token === "i1001") return png.buffer;
        // i1002 answers only after the session ended.
        return new Promise<ArrayBuffer>((resolve) => late.push(() => resolve(png.slice().buffer)));
      }) as typeof bridge.invoke;
      let made = 0;
      vi.spyOn(URL, "createObjectURL").mockImplementation(() => `blob:gwar/${++made}`);
      const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
      const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
      await conn.connect();
      const session = calls.find((c) => c.command === "ts_connect")?.args?.session;
      const emit = (ev: string, d: unknown) => listeners.get("ts://event")?.({ session, frame: JSON.stringify({ ev, d }) } as never);
      const channel = { id: 1, parent: null, name: "Lobby", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false };
      emit("channel.updated", { ...channel, icon: "i1001" });
      emit("channel.created", { ...channel, id: 2, name: "Other", icon: "i1002" });
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(useTsImages.getState().urls).toEqual({ i1001: "blob:gwar/1" });
      if (terminal) emit("disconnected", { reason: { kind: "kicked", by: "admin", reason: null } });
      listeners.get("ts://closed")?.({ session } as never);
      expect(conn.status.state).toBe(terminal ? "closed" : "reconnecting");
      expect(useTsImages.getState().urls).toEqual({});
      expect(revoke).toHaveBeenCalledWith("blob:gwar/1");
      for (const answer of late) answer();
      await new Promise((resolve) => setTimeout(resolve, 0));
      expect(useTsImages.getState().urls).toEqual({});
      expect(made).toBe(1);
      conn.close();
      vi.restoreAllMocks();
    }
  });
});
