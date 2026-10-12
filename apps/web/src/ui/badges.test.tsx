// @vitest-environment happy-dom
import { act, useLayoutEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { acceptHandle, resetHandlePins } from "../net/handle-pins";
import { useSettings } from "../state/settings";
import { ConnectBadge } from "./badges";
import { useConnectHandle } from "./useConnectHandle";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => "mallory" }));
const member = { uid: "uid" };

describe("Connect badge", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    localStorage.clear(); resetHandlePins();
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
  it("marks a handle seen on a different account before instead of verifying it", async () => {
    acceptHandle("mallory", "someone-else");
    await show("mallory");
    expect(container.querySelector('[data-testid="connect-badge"]')).toBeNull();
    const changed = container.querySelector('[data-testid="connect-badge-changed"]')!;
    expect(changed.textContent).toBe("@mallory");
    expect(changed.getAttribute("aria-label")).toContain("different account");
  });
  it("verifies again once the user accepts the account", async () => {
    acceptHandle("mallory", "someone-else");
    await show("mallory");
    await act(async () => acceptHandle("mallory", "uid"));
    expect(container.querySelector('[data-testid="connect-badge"]')).not.toBeNull();
  });
  it("pins the first account it verifies", async () => {
    await show("Alice");
    await act(async () => root.render(<ConnectBadge member={{ uid: "other" }} name="Alice" />));
    expect(container.querySelector('[data-testid="connect-badge-changed"]')).not.toBeNull();
  });
  it("never shows two accounts as verified for the same new handle, not even for one commit", async () => {
    const commits: string[] = [];
    function Probe({ uid }: { uid: string }) {
      const connect = useConnectHandle({ uid });
      useLayoutEffect(() => { commits.push(`${uid}:${connect?.status ?? "none"}`); });
      return null;
    }
    await act(async () => root.render(<><Probe uid="A" /><Probe uid="B" /></>));
    expect(commits).not.toContain("B:verified");
    expect(commits.slice(-2)).toEqual(["A:verified", "B:changed"]);
  });
});
