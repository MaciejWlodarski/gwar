import { describe, expect, it } from "vitest";
import { displayText, tokenizeText } from "./text";

describe("tokenizeText", () => {
  it("returns plain text untouched, including markup", () => {
    expect(tokenizeText("<img src=x onerror=alert(1)> hi")).toEqual([{ kind: "text", value: "<img src=x onerror=alert(1)> hi" }]);
  });

  it("linkifies http(s) and www urls and trims sentence punctuation", () => {
    expect(tokenizeText("see https://a.example/x?y=1, ok. And www.b.org!")).toEqual([
      { kind: "text", value: "see " },
      { kind: "link", value: "https://a.example/x?y=1", href: "https://a.example/x?y=1" },
      { kind: "text", value: ", ok. And " },
      { kind: "link", value: "www.b.org", href: "https://www.b.org" },
      { kind: "text", value: "!" },
    ]);
  });

  it("keeps balanced parentheses", () => {
    const t = tokenizeText("(https://en.wikipedia.org/wiki/Foo_(bar))");
    expect(t[1]).toMatchObject({ kind: "link", value: "https://en.wikipedia.org/wiki/Foo_(bar)" });
  });

  it("never links javascript: urls", () => {
    expect(tokenizeText("javascript:alert(1) data:text/html,x")).toEqual([{ kind: "text", value: "javascript:alert(1) data:text/html,x" }]);
  });
});

describe("displayText", () => {
  it("drops bidi formatting and turns control characters into spaces on one line", () => {
    expect(displayText("evil\u202egnp.exe\u2066x\u2069")).toBe("evilgnp.exex");
    expect(displayText("a\u0000b\nc\td  e ")).toBe("a b c d e");
    expect(displayText("Zażółć 🎧 مرحبا")).toBe("Zażółć 🎧 مرحبا");
  });
  it("keeps newlines and tabs in text with lines", () => {
    expect(displayText("one\r\ntwo\rthree\n\tfour\u0007\u202e", true)).toBe("one\ntwo\nthree\n\tfour ");
  });
});
