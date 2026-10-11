import { describe, expect, it } from "vitest";
import { buildInviteLink, parseInviteLink, readInviteFromSearch } from "./invite";
const key = "A".repeat(43);
describe("vc/3 invites", () => {
  it("carries the full key and exact endpoint through sharing and pasting", () => {
    const link = buildInviteLink({ webOrigin: "https://web.example.com", serverEndpoint: "wss://Voice.Example.:8443/custom/ws", serverKey: key, code: "abc123" });
    expect(parseInviteLink(link)).toEqual({ code: "abc123", server: "wss://voice.example:8443/custom/ws", serverKey: key });
    expect(readInviteFromSearch(new URL(link).search)).toEqual({ target: parseInviteLink(link), rest: "" });
  });
  it("retains unrelated query parameters and strips invite secrets and the key", () => {
    expect(readInviteFromSearch(`?x=1&invite=abc&server_key=${key}`)).toEqual({ target: { code: "abc", serverKey: key }, rest: "?x=1" });
  });
  it.each(["?invite=abc", "?invite=abc&server_key=bad", `?invite=%3Cscript%3E&server_key=${key}`, `?invite=abc&server_key=${key}%3D`])("rejects malformed and fingerprint-free links: %s", (search) => {
    expect(readInviteFromSearch(search).target).toBeNull();
    expect(parseInviteLink(`https://web.example/${search}`)).toBeNull();
  });
  it("uses the link's origin when an invite omits the server", () => {
    expect(parseInviteLink(`https://server.example/?invite=abc&server_key=${key}`)).toEqual({ code: "abc", server: "https://server.example", serverKey: key });
  });
});
