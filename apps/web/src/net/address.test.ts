import { describe, expect, it } from "vitest";
import { parseServerAddress, sameOriginUrl, type PageContext } from "./address";

const http: PageContext = { protocol: "http:", hostname: "localhost", port: "5173" };
const https: PageContext = { protocol: "https:", hostname: "voice.example.com", port: "" };
const tauri: PageContext = { protocol: "tauri:", hostname: "localhost", port: "" };

const url = (input: string, page: PageContext) => {
  const r = parseServerAddress(input, page);
  return r.ok ? r.value.url : r.error;
};

describe("parseServerAddress", () => {
  it("bare host on an http page gets ws and the default port", () => {
    expect(url("voice.example.com", http)).toBe("ws://voice.example.com:8790/ws");
    expect(url("  1.2.3.4  ", http)).toBe("ws://1.2.3.4:8790/ws");
  });

  it("keeps an explicit port", () => {
    expect(url("1.2.3.4:9000", http)).toBe("ws://1.2.3.4:9000/ws");
    expect(url("voice.example.com:8790", https)).toBe("wss://voice.example.com:8790/ws");
  });

  it("bare host on an https page is wss on the default https port", () => {
    expect(url("other.example.org", https)).toBe("wss://other.example.org/ws");
  });

  it("the page's own host reuses the page port", () => {
    expect(url("localhost", http)).toBe("ws://localhost:5173/ws");
    expect(url("voice.example.com", https)).toBe("wss://voice.example.com/ws");
  });

  it("treats non-http pages (tauri) as plain http", () => {
    expect(url("example.com", tauri)).toBe("ws://example.com:8790/ws");
  });

  it("honours explicit schemes and maps http(s) to ws(s)", () => {
    expect(url("wss://x.example/ws", http)).toBe("wss://x.example/ws");
    expect(url("ws://x.example", http)).toBe("ws://x.example/ws");
    expect(url("https://x.example", http)).toBe("wss://x.example/ws");
    expect(url("http://x.example:81", https)).toBe("mixed_content");
    expect(url("ws://localhost:8790", https)).toBe("ws://localhost:8790/ws");
  });

  it("keeps explicit paths and drops query/hash", () => {
    expect(url("wss://x.example/custom/ws?a=1#b", http)).toBe("wss://x.example/custom/ws");
  });

  it("handles IPv6", () => {
    expect(url("[::1]:8790", http)).toBe("ws://[::1]:8790/ws");
    expect(url("::1", http)).toBe("ws://[::1]:8790/ws");
    expect(url("wss://[2001:db8::1]/ws", http)).toBe("wss://[2001:db8::1]/ws");
  });

  it("rejects garbage", () => {
    expect(url("", http)).toBe("empty");
    expect(url("   ", http)).toBe("empty");
    expect(url("ftp://x", http)).toBe("scheme");
    expect(url("javascript:alert(1)", http)).toBe("scheme");
    expect(url("host:99999", http)).toBe("invalid");
    expect(url("a b", http)).toBe("invalid");
    expect(url("user@host", http)).toBe("invalid");
  });

  it("produces short labels", () => {
    const r = parseServerAddress("voice.example.com", https);
    expect(r.ok && r.value.label).toBe("voice.example.com");
    const r2 = parseServerAddress("1.2.3.4", http);
    expect(r2.ok && r2.value.label).toBe("1.2.3.4:8790");
  });
});

describe("sameOriginUrl", () => {
  it("mirrors the page", () => {
    expect(sameOriginUrl(https)).toBe("wss://voice.example.com/ws");
    expect(sameOriginUrl(http)).toBe("ws://localhost:5173/ws");
  });
});
