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

function fakeBridge() {
  const listeners = new Map<string, (p: never) => void>();
  const calls: Array<{ command: string; args?: Record<string, unknown> }> = [];
  const bridge: TauriBridge = {
    invoke: (async (command: string, args?: Record<string, unknown>) => {
      calls.push({ command, args });
      if (command === "ts_connect") {
        // An event races the welcome reply.
        listeners.get("ts://event")?.({ session: args?.session, frame: JSON.stringify({ ev: "voice.talking", d: { client: 1, talking: true } }) } as never);
        return { session: 1 };
      }
      if (command === "ts_request") throw { code: "forbidden", message: "no" };
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
    const conn = new TsConnection({ address: "h:9987", nickname: "n", bridge: async () => bridge });
    const seen: string[] = [];
    conn.onWelcome(() => seen.push("welcome"));
    conn.onEvent((e) => seen.push(e.ev));
    await conn.connect();
    expect(seen).toEqual(["welcome", "voice.talking"]);
    expect(conn.isOnline).toBe(true);
    await expect(conn.request("ping", {})).rejects.toBeInstanceOf(RequestError);
    expect(calls.find((c) => c.command === "ts_connect")?.args).toMatchObject({ address: "h:9987", nickname: "n", password: null });
    conn.close();
    expect(calls.at(-1)?.command).toBe("ts_disconnect");
  });
});
