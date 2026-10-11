// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { ConnectBadge } from "./badges";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => "mallory" }));
const member = { uid: "uid" };

describe("Connect badge", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
  });
  const show = (name?: string) => act(async () => root.render(<ConnectBadge member={member} name={name} />));

  it("never vouches for a nickname that is not the handle", async () => {
    await show("Alice");
    const badge = container.querySelector('[data-testid="connect-badge"]')!;
    expect(badge.textContent).toBe("@mallory");
    expect(badge.getAttribute("aria-label")).toContain("mallory");
  });
  for (const name of ["mallory", "Mallory", " @mallory "]) {
    it(`stays a bare check next to the handle itself (${JSON.stringify(name)})`, async () => {
      await show(name);
      expect(container.querySelector('[data-testid="connect-badge"]')!.textContent).toBe("");
    });
  }
  it("shows the handle for look-alike nicknames", async () => {
    await show("mallоry"); // Cyrillic о
    expect(container.textContent).toBe("@mallory");
  });
});
