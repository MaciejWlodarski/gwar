import { describe, expect, it } from "vitest";
import { absoluteUrl, isPlaintextRemote, parseServerAddress, sameOriginUrl, type PageContext } from "./address";

const http: PageContext = { protocol: "http:", hostname: "localhost", port: "5173" };
const https: PageContext = { protocol: "https:", hostname: "voice.example.com", port: "" };
const tauri: PageContext = { protocol: "tauri:", hostname: "localhost", port: "" };
const windowsApp: PageContext = { protocol: "http:", hostname: "tauri.localhost", port: "" };

const url = (input: string, page: PageContext) => {
  const r = parseServerAddress(input, page);
  return r.ok ? r.value.url : r.error;
};

describe("parseServerAddress", () => {
  it("bare host on an http page gets wss", () => {
    expect(url("voice.example.com", http)).toBe("wss://voice.example.com/ws");
    expect(url("  1.2.3.4  ", http)).toBe("wss://1.2.3.4/ws");
  });

  it("keeps an explicit port", () => {
    expect(url("1.2.3.4:9000", http)).toBe("wss://1.2.3.4:9000/ws");
    expect(url("voice.example.com:8790", https)).toBe("wss://voice.example.com:8790/ws");
  });

  it("bare host on an https page is wss on the default https port", () => {
    expect(url("other.example.org", https)).toBe("wss://other.example.org/ws");
  });

  it("a hosted https page connects to hostnames over wss and refuses plain ws", () => {
    expect(url("play.example.org", https)).toBe("wss://play.example.org/ws");
    expect(url("play.example.org:8443", https)).toBe("wss://play.example.org:8443/ws");
    expect(url("https://play.example.org", https)).toBe("wss://play.example.org/ws");
    expect(url("ws://play.example.org", https)).toBe("mixed_content");
    expect(url("http://play.example.org:8790", https)).toBe("mixed_content");
    expect(url("ws://192.168.1.10:8790", https)).toBe("mixed_content");
  });

  it("an https page may reach a server on this machine over plain ws", () => {
    expect(url("localhost", https)).toBe("ws://localhost:8790/ws");
    expect(url("127.0.0.1:9000", https)).toBe("ws://127.0.0.1:9000/ws");
    expect(url("http://localhost:8790", https)).toBe("ws://localhost:8790/ws");
    expect(url("wss://localhost", https)).toBe("wss://localhost/ws");
  });

  it("the page's own host reuses the page port", () => {
    expect(url("localhost", http)).toBe("ws://localhost:5173/ws");
    expect(url("voice.example.com", https)).toBe("wss://voice.example.com/ws");
  });

  it("the desktop app reaches domain names over wss and local hosts over plain ws", () => {
    expect(url("voice.example.com", tauri)).toBe("wss://voice.example.com/ws");
    expect(url("voice.example.com", windowsApp)).toBe("wss://voice.example.com/ws");
    expect(url("voice.example.com:8790", tauri)).toBe("wss://voice.example.com:8790/ws");
    expect(url("1.2.3.4", tauri)).toBe("wss://1.2.3.4/ws");
    expect(url("localhost", tauri)).toBe("ws://localhost:8790/ws");
    expect(url("::1", tauri)).toBe("ws://[::1]:8790/ws");
    expect(url("nas", tauri)).toBe("wss://nas/ws");
    expect(url("pc.local", windowsApp)).toBe("wss://pc.local/ws");
    expect(url("ws://voice.example.com", tauri)).toBe("insecure");
    expect(url("https://192.168.1.10", tauri)).toBe("wss://192.168.1.10/ws");
  });

  it("honours explicit schemes and maps http(s) to ws(s)", () => {
    expect(url("wss://x.example/ws", http)).toBe("wss://x.example/ws");
    expect(url("ws://x.example", http)).toBe("insecure");
    expect(url("https://x.example", http)).toBe("wss://x.example/ws");
    expect(url("http://x.example:81", https)).toBe("mixed_content");
    expect(url("ws://localhost:8790", https)).toBe("ws://localhost:8790/ws");
  });

  it("keeps explicit paths and rejects query/hash", () => {
    expect(url("wss://x.example/custom/ws", http)).toBe("wss://x.example/custom/ws");
    expect(url("wss://x.example/custom/ws?a=1#b", http)).toBe("invalid");
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
    expect(r2.ok && r2.value.label).toBe("1.2.3.4");
  });
});

describe("sameOriginUrl", () => {
  it("mirrors the page", () => {
    expect(sameOriginUrl(https)).toBe("wss://voice.example.com/ws");
    expect(sameOriginUrl(http)).toBe("ws://localhost:5173/ws");
  });
});

// File URLs are capabilities on the selected server, never arbitrary URLs.
describe("file URL confinement", () => {
  it("accepts only the download and upload routes for their methods", () => {
    expect(absoluteUrl("https://voice.example", "/files/abc/photo%20one.png")).toBe("https://voice.example/files/abc/photo%20one.png");
    expect(absoluteUrl("https://voice.example", "/files/abc/v1..2%20100%25.zip")).toBe("https://voice.example/files/abc/v1..2%20100%25.zip");
    expect(absoluteUrl("http://192.168.1.2:8790", "/api/files/abc?token=abc", "PUT")).toBe("http://192.168.1.2:8790/api/files/abc?token=abc");
    expect(() => absoluteUrl("https://voice.example", "/api/files/a?token=x")).toThrow();
    expect(() => absoluteUrl("https://voice.example", "/files/a/b", "PUT")).toThrow();
  });
  it("rejects absolute, traversal, encoded, credential and odd-scheme inputs", () => {
    for (const path of ["https://voice.example/files/a/b", "http://127.0.0.1/a", "//evil.example/files/a/b", "/files/a/../b", "/files/a/%2e%2e", "/files/a/%2fb", "/files/a/%5cb", "/files/a/b#c", "/files/a/b?q=1", "file:///etc/passwd", "javascript:alert(1)"]) {
      expect(() => absoluteUrl("https://voice.example", path), path).toThrow();
    }
    expect(() => absoluteUrl(null, "/files/a/b")).toThrow();
    expect(() => absoluteUrl("https://u:p@voice.example", "/files/a/b")).toThrow();
  });
});

describe("explicit desktop LAN opt-in", () => {
  it("applies only to the chosen server and never to web connections", () => {
    expect(parseServerAddress("ws://192.168.1.2:8790", tauri).ok).toBe(false);
    expect(parseServerAddress("ws://192.168.1.2:8790", tauri, true)).toMatchObject({ ok: true, value: { secure: false } });
    expect(parseServerAddress("ws://192.168.1.2:8790", http, true).ok).toBe(false);
    expect(parseServerAddress("ws://127.attacker.example", tauri).ok).toBe(false);
  });
});

describe("isPlaintextRemote", () => {
  it("flags ws:// to another machine only", () => {
    expect(isPlaintextRemote("ws://192.168.1.20:8790/ws")).toBe(true);
    expect(isPlaintextRemote("ws://voice.example.com/ws")).toBe(true);
    expect(isPlaintextRemote("ws://127.0.0.1:8790/ws")).toBe(false);
    expect(isPlaintextRemote("ws://localhost:8790/ws")).toBe(false);
    expect(isPlaintextRemote("ws://[::1]:8790/ws")).toBe(false);
    expect(isPlaintextRemote("wss://192.168.1.20/ws")).toBe(false);
  });
});

describe("addressLabel", () => {
  const page = { protocol: "https:", hostname: "gwar.app", port: "" };
  it("shows a secure default URL as its host", async () => {
    const { addressLabel } = await import("./address");
    expect(addressLabel("wss://gwar.app/ws", page)).toBe("gwar.app");
    expect(addressLabel("wss://voice.example.com:8443/ws", page)).toBe("voice.example.com:8443");
    expect(addressLabel("wss://voice.example.com/other", page)).toBe("voice.example.com/other");
  });
  it("keeps anything that would not mean the same URL again", async () => {
    const { addressLabel } = await import("./address");
    expect(addressLabel("ws://192.168.1.10:8790/ws", page)).toBe("ws://192.168.1.10:8790/ws");
    expect(addressLabel("not a url", page)).toBe("not a url");
  });
});
