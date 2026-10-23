import { describe, expect, it } from "vitest";
import type { PinStore } from "../net/server-pins";
import { serverKey } from "./server-key";

const web = { protocol: "https:", hostname: "gwar.app", port: "" };
const app = { protocol: "tauri:", hostname: "localhost", port: "" };
const key = "A".repeat(43);
const pins = (byEndpoint: Record<string, string>): PinStore => ({
  load: (endpoint) => (byEndpoint[endpoint] ? { server_pk: byEndpoint[endpoint], first_seen: 1 } : undefined),
  commit: async () => {},
  forget: async () => {},
});

describe("serverKey", () => {
  it("treats every spelling of one Gwar address as one server", () => {
    const same = ["gwar.app", " GWAR.app ", "gwar.app/", "wss://gwar.app/ws", "https://gwar.app", "gwar.app:443"];
    for (const page of [web, app]) {
      const keys = new Set(same.map((address) => serverKey({ address }, page, null)));
      expect(keys).toEqual(new Set(["gwar|wss://gwar.app/ws"]));
    }
    expect(serverKey({ address: "voice.gwar.app" }, web, null)).not.toBe(serverKey({ address: "gwar.app" }, web, null));
    expect(serverKey({ address: "gwar.app/other" }, web, null)).not.toBe(serverKey({ address: "gwar.app" }, web, null));
  });

  it("keeps the kinds apart and matches TeamSpeak by host and port", () => {
    expect(serverKey({ kind: "teamspeak", address: "ts.example.com" }, app, null)).toBe(serverKey({ kind: "teamspeak", address: "TS.example.com:9987" }, app, null));
    expect(serverKey({ kind: "teamspeak", address: "ts3server://ts.example.com" }, app, null)).toBe("teamspeak|ts.example.com:9987");
    expect(serverKey({ kind: "teamspeak", address: "ts.example.com:9988" }, app, null)).not.toBe("teamspeak|ts.example.com:9987");
    expect(serverKey({ kind: "teamspeak", address: "gwar.app" }, app, null)).not.toBe(serverKey({ address: "gwar.app" }, app, null));
  });

  it("names a LAN server the same with or without the scheme typed", () => {
    expect(serverKey({ address: "192.168.1.10", insecureLan: true }, app, null)).toBe(serverKey({ address: "ws://192.168.1.10:8790/ws" }, app, null));
    // Even where it cannot be opened (a web page), the explicit address still names one server.
    expect(serverKey({ address: "ws://192.168.1.10:8790/ws" }, web, null)).toBe("gwar|ws://192.168.1.10:8790/ws");
    expect(serverKey({ address: "not a server" }, web, null)).toBe("gwar|not a server");
  });

  it("knows a server it has connected to by its pinned key under any address", () => {
    const store = pins({ "wss://gwar.app/ws": key, "wss://203.0.113.7/ws": key });
    expect(serverKey({ address: "gwar.app" }, web, store)).toBe(`gwar-key|${key}`);
    expect(serverKey({ address: "203.0.113.7" }, web, store)).toBe(`gwar-key|${key}`);
    expect(serverKey({ address: "other.example" }, web, store)).toBe("gwar|wss://other.example/ws");
    const broken: PinStore = { ...store, load: () => { throw new Error("invalid server pin"); } };
    expect(serverKey({ address: "gwar.app" }, web, broken)).toBe("gwar|wss://gwar.app/ws");
  });
});
