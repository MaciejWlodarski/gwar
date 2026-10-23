// @vitest-environment happy-dom
import "fake-indexeddb/auto";
import { act, useLayoutEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { CONNECT_URL } from "../lib/official";
import { createLocalState, localState } from "../lib/local-state";
import { acceptHandle, resetHandlePins } from "../net/handle-pins";
import { useSettings } from "../state/settings";
import { ConnectBadge } from "./badges";
import { useConnectHandle } from "./useConnectHandle";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => "mallory" }));
const member = { uid: "uid" };

describe("Connect badge", () => {
  let container: HTMLDivElement, root: Root;
  const pin = `gwar.connect.handle-pin:${CONNECT_URL}:mallory`;
  beforeAll(() => localState.load());
  beforeEach(async () => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    useSettings.setState({ language: "en" });
    await localState.update(pin, () => null); resetHandlePins();
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals();
  });
  /** Renders, and lets the first sighting's pin commit (an IndexedDB transaction) and reach the view. */
  const rendered = async (node: React.ReactNode) => {
    await act(async () => root.render(node));
    await act(async () => { await vi.waitFor(() => expect(localState.get(pin)).toBeDefined()); });
  };
  const show = (name?: string) => rendered(<ConnectBadge member={member} name={name} />);

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
    await acceptHandle("mallory", "someone-else");
    await show("mallory");
    expect(container.querySelector('[data-testid="connect-badge"]')).toBeNull();
    const changed = container.querySelector('[data-testid="connect-badge-changed"]')!;
    expect(changed.textContent).toBe("@mallory");
    expect(changed.getAttribute("aria-label")).toContain("different account");
  });
  it("verifies again once the user accepts the account", async () => {
    await acceptHandle("mallory", "someone-else");
    await show("mallory");
    await act(async () => acceptHandle("mallory", "uid"));
    expect(container.querySelector('[data-testid="connect-badge"]')).not.toBeNull();
  });
  it("pins the first account it verifies", async () => {
    await show("Alice");
    await rendered(<ConnectBadge member={{ uid: "other" }} name="Alice" />);
    expect(container.querySelector('[data-testid="connect-badge-changed"]')).not.toBeNull();
  });
  it("never shows two accounts as verified for the same new handle, not even for one commit", async () => {
    const commits: string[] = [];
    function Probe({ uid }: { uid: string }) {
      const connect = useConnectHandle({ uid });
      useLayoutEffect(() => { commits.push(`${uid}:${connect?.status ?? "none"}`); });
      return null;
    }
    await rendered(<><Probe uid="A" /><Probe uid="B" /></>);
    expect(commits).not.toContain("B:verified");
    expect(commits.slice(-2)).toEqual(["A:verified", "B:changed"]);
  });
  it("shows the handle again after another tab cleared the pins", async () => {
    await show("Alice");
    expect(container.querySelector('[data-testid="connect-badge"]')).not.toBeNull();
    const other = createLocalState();
    await other.load();
    await other.update(pin, () => null);
    // This tab hears of it, finds the handle unpinned and pins it again.
    await act(async () => { await vi.waitFor(() => expect(other.get(pin)).toBeDefined()); });
    other.close();
    expect(container.querySelector('[data-testid="connect-badge"]')).not.toBeNull();
  });
});
