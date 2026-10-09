import { describe, expect, it } from "vitest";
import { absoluteUrl, httpOriginFromWsUrl } from "./address";
import { OFFICIAL_WEB_ORIGIN } from "../lib/official";
import { buildInviteLink, parseInviteLink, readInviteFromSearch } from "./invite";

describe("readInviteFromSearch", () => {
  it("reads the code and optional server and returns the rest of the query", () => {
    expect(readInviteFromSearch("?invite=abc_DEF-1")).toEqual({ target: { code: "abc_DEF-1" }, rest: "" });
    expect(readInviteFromSearch("?x=1&invite=abc&server=voice.example.com%3A8790")).toEqual({
      target: { code: "abc", server: "voice.example.com:8790" },
      rest: "?x=1",
    });
  });
  it("ignores missing or malformed codes", () => {
    expect(readInviteFromSearch("").target).toBeNull();
    expect(readInviteFromSearch("?invite=<script>").target).toBeNull();
    expect(readInviteFromSearch("?invite=").rest).toBe("");
  });
});

describe("parseInviteLink", () => {
  it("takes the server from the link's own host", () => {
    expect(parseInviteLink("https://chat.example.com/?invite=abc123")).toEqual({ code: "abc123", server: "chat.example.com" });
    expect(parseInviteLink("  http://10.0.0.5:8790/?invite=abc123 ")).toEqual({ code: "abc123", server: "10.0.0.5:8790" });
  });
  it("prefers an explicit server parameter", () => {
    expect(parseInviteLink("https://web.example.com/?invite=abc&server=voice.example.com:8790")).toEqual({
      code: "abc",
      server: "voice.example.com:8790",
    });
  });
  it("is null for anything else", () => {
    expect(parseInviteLink("voice.example.com")).toBeNull();
    expect(parseInviteLink("https://chat.example.com/")).toBeNull();
    expect(parseInviteLink("javascript:alert(1)")).toBeNull();
  });
});

describe("buildInviteLink", () => {
  it("is just the web app's origin when it is the server too", () => {
    expect(buildInviteLink({ webOrigin: "https://chat.example.com", serverOrigin: "https://chat.example.com", code: "abc" })).toBe(
      "https://chat.example.com/?invite=abc",
    );
  });
  it("names the server when it is on another origin", () => {
    expect(buildInviteLink({ webOrigin: "https://gwar.maciejwlodarski.com", serverOrigin: "https://play.example.org", code: "abc" })).toBe(
      "https://gwar.maciejwlodarski.com/?server=play.example.org&invite=abc",
    );
    expect(buildInviteLink({ webOrigin: "http://127.0.0.1:5173", serverOrigin: "http://127.0.0.1:8799", code: "abc" })).toBe(
      "http://127.0.0.1:5173/?server=127.0.0.1%3A8799&invite=abc",
    );
    expect(buildInviteLink({ webOrigin: "https://web.example.com", serverOrigin: "https://srv.example.com:8443", code: "abc" })).toBe(
      "https://web.example.com/?server=srv.example.com%3A8443&invite=abc",
    );
  });
  it("keeps the scheme when the server's differs from the web app's", () => {
    expect(buildInviteLink({ webOrigin: "http://localhost:5173", serverOrigin: "https://srv.example.com", code: "abc" })).toBe(
      "http://localhost:5173/?server=https%3A%2F%2Fsrv.example.com&invite=abc",
    );
  });
  it("always points at the web app, never at the server (desktop app)", () => {
    expect(buildInviteLink({ webOrigin: OFFICIAL_WEB_ORIGIN, serverOrigin: "https://chat.example.com", code: "abc" })).toBe(
      `${OFFICIAL_WEB_ORIGIN}/?server=chat.example.com&invite=abc`,
    );
  });
  it("round-trips through the reader", () => {
    const link = buildInviteLink({ webOrigin: "http://web.test", serverOrigin: "http://srv.test:8790", code: "zz9" });
    expect(readInviteFromSearch(new URL(link).search).target).toEqual({ code: "zz9", server: "srv.test:8790" });
    expect(parseInviteLink(link)).toEqual({ code: "zz9", server: "srv.test:8790" });
  });
});

describe("server HTTP origin", () => {
  it("maps the websocket URL", () => {
    expect(httpOriginFromWsUrl("ws://127.0.0.1:8799/ws")).toBe("http://127.0.0.1:8799");
    expect(httpOriginFromWsUrl("wss://voice.example.com/ws")).toBe("https://voice.example.com");
    expect(httpOriginFromWsUrl("http://x")).toBeNull();
  });
  it("makes relative paths absolute", () => {
    expect(absoluteUrl("http://h:1", "/files/a/b.png")).toBe("http://h:1/files/a/b.png");
    expect(absoluteUrl("http://h:1/", "files/a")).toBe("http://h:1/files/a");
    expect(absoluteUrl(null, "/files/a")).toBe("/files/a");
    expect(absoluteUrl("http://h", "https://cdn/x")).toBe("https://cdn/x");
  });
});
