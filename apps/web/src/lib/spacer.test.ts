import { describe, expect, it } from "vitest";
import { fillText, parseSpacer } from "./spacer";

describe("parseSpacer", () => {
  it("reads TeamSpeak's spacer kinds", () => {
    expect(parseSpacer("[spacer0]Games", true)).toEqual({ align: "left", text: "Games" });
    expect(parseSpacer("[lspacer]Games", true)).toEqual({ align: "left", text: "Games" });
    expect(parseSpacer("[cspacer1]Welcome", true)).toEqual({ align: "center", text: "Welcome" });
    expect(parseSpacer("[rspacer]x", true)).toEqual({ align: "right", text: "x" });
    expect(parseSpacer("[*spacer9]-", true)).toEqual({ align: "fill", text: "-" });
    expect(parseSpacer("[cspacer]", true)).toEqual({ align: "center", text: "" });
  });

  it("leaves ordinary and nested channels alone", () => {
    expect(parseSpacer("Lobby", true)).toBeNull();
    expect(parseSpacer("[xspacer]x", true)).toBeNull();
    expect(parseSpacer("x [spacer]", true)).toBeNull();
    expect(parseSpacer("[cspacer]Welcome", false)).toBeNull();
  });

  it("repeats fill text to the requested length", () => {
    expect(fillText("-.", 5)).toBe("-.-.-");
    expect(fillText("", 5)).toBe("");
  });
});
