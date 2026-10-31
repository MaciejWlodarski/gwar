import { describe, expect, it } from "vitest";
import { buildInviteLink, parseInviteLink, readInviteFromSearch } from "./invite";
const key = "A".repeat(43);
describe("gwar/4 invites", () => {
  it("carries the full key and exact endpoint through sharing and pasting", () => {
    const link = buildInviteLink({ webOrigin: "https://web.example.com", serverEndpoint: "wss://Voice.Example.:8443/custom/ws", serverKey: key, code: "abc123" });
    expect(parseInviteLink(link)).toEqual({ code: "abc123", server: "wss://voice.example:8443/custom/ws", serverKey: key });
    expect(readInviteFromSearch(new URL(link).search)).toEqual({ target: parseInviteLink(link), rest: "" });
  });
  it("carries the verified owner, and refuses a malformed one", () => {
    const owner = "B".repeat(26) + "A";
    const link = buildInviteLink({ webOrigin: "https://web.example.com", serverEndpoint: "wss://voice.example/ws", serverKey: key, code: "abc123", owner });
    expect(parseInviteLink(link)?.owner).toBe(owner);
    expect(readInviteFromSearch(new URL(link).search).rest).toBe("");
    expect(readInviteFromSearch(`?invite=abc&server_key=${key}&owner=bad`).target).toBeNull();
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

describe("untrusted desktop invites", () => {
  it("round-trips an exact destination, key, owner and inviter-supplied name", async () => {
    const { desktopInviteLink, parseDesktopInvite } = await import("./invite");
    const target = { code: "abc", serverKey: key, server: "wss://voice.example/custom/ws", owner: "B".repeat(26) + "A", name: "Nasza ekipa" };
    expect(parseDesktopInvite(desktopInviteLink(target)!)).toEqual(target);
    const web = buildInviteLink({ webOrigin: "https://web.example", serverEndpoint: target.server, ...target });
    expect(parseInviteLink(web)).toEqual(target);
  });
  it.each([
    "gwar://invite/a/../?invite=abc", "gwar://invite/%2e/?invite=abc",
    "gwar://command/?invite=abc", "gwar://invite/path?invite=abc", "gwar://user@invite/?invite=abc", "gwar://invite:42/?invite=abc",
    "gwar://invite/?invite=abc#fragment", "gwar://invite/?invite=abc&server=javascript:alert(1)",
    "gwar://invite/?invite=abc&server=wss://user:pass@evil.example/ws", "gwar://invite/?invite=abc&server=wss://room/ws?token=secret",
    "gwar://invite/?invite=abc&server=wss://room/ws&invite=def", "gwar://invite/?invite=abc&server=wss://room/ws&execute=join",
    "gwar://invite/?invite=abc&server=wss://room/ws&name=%E2%80%AEspoof", "gwar://invite/?invite=abc&server=wss://room/ws&owner=bad",
    "gwar://invite/?invite=abc", "gwar://invite/\\?invite=abc", "gwar://invite/?invite=abc&server=" + "x".repeat(4096),
  ])("refuses malformed routes without any network side effect: %s", async prefix => {
    const { parseDesktopInvite } = await import("./invite");
    expect(parseDesktopInvite(`${prefix}&server_key=${key}`)).toBeNull();
  });
  it.each(["javascript:alert(1)", "https://a:b@room/", "https://room/path", "https://room/#fragment"])("refuses misleading pasted link wrappers: %s", prefix => {
    expect(parseInviteLink(`${prefix}${prefix.includes("#") ? "" : "?"}invite=abc&server_key=${key}`)).toBeNull();
  });
});
