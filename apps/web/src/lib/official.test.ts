import { describe, expect, it } from "vitest";
import { OFFICIAL_WEB_ORIGIN, webOrigin } from "./official";

describe("webOrigin", () => {
  it("is the page's own origin on http(s) pages", () => {
    expect(webOrigin({ protocol: "https:", origin: "https://app.example.org" })).toBe("https://app.example.org");
    expect(webOrigin({ protocol: "http:", origin: "http://127.0.0.1:5173" })).toBe("http://127.0.0.1:5173");
  });
  it("is the official web app in the desktop app", () => {
    expect(webOrigin({ protocol: "tauri:", origin: "tauri://localhost" })).toBe(OFFICIAL_WEB_ORIGIN);
    expect(webOrigin({ protocol: "http:", origin: "http://tauri.localhost" })).toBe("http://tauri.localhost");
  });
});
