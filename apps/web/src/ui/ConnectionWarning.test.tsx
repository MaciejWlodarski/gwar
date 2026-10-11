import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { ConnectionWarning } from "./ConnectionWarning";

describe("persistent plaintext connection warning", () => {
  it.each(["online", "reconnecting"] as const)("stays red and visible while %s, outside dialogs and drawers", (phase) => {
    const html = renderToStaticMarkup(<ConnectionWarning phase={phase} insecure />);
    expect(html).toContain('role="alert"');
    expect(html).toContain("bg-red-700");
    expect(html).toContain("connection can be impersonated or taken over");
  });
  it("hides for secure connections and after disconnect", () => {
    expect(renderToStaticMarkup(<ConnectionWarning phase="online" insecure={false} />)).toBe("");
    expect(renderToStaticMarkup(<ConnectionWarning phase="idle" insecure />)).toBe("");
  });
});
