import { describe, expect, it } from "vitest";
import { tokenizeText } from "./text";

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
