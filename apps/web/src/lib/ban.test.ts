import { describe, expect, it } from "vitest";
import { parseBanMessage } from "./ban";

describe("parseBanMessage", () => {
  it("reads the end time and the reason out of the server's sentence", () => {
    expect(parseBanMessage("you are banned from this server until 1893456000000 (unix ms): spamming links")).toEqual({
      until: 1893456000000,
      reason: "spamming links",
    });
  });
  it("copes with permanent bans and bans without a reason", () => {
    expect(parseBanMessage("you are banned from this server: rude")).toEqual({ until: null, reason: "rude" });
    expect(parseBanMessage("you are banned from this server")).toEqual({ until: null, reason: null });
  });
});
