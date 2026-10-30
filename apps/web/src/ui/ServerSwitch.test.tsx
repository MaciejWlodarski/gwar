// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import type { Bookmark } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { useBookmarkOpener } from "./nav";

const connectInteractive = vi.hoisted(() => vi.fn(async () => false));
vi.mock("../state/controller", () => ({ controller: { connectInteractive }, bookmarkParams: (b: Bookmark) => b }));

it("keeps Home underneath a switch confirmation, and opens an active reconnect without restarting it", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSession.setState({ phase: "reconnecting", kind: "gwar", address: "current.example" });
  useUi.setState({ section: "home" });
  let open!: ReturnType<typeof useBookmarkOpener>["open"];
  function Fixture() { open = useBookmarkOpener().open; return null; }
  const host = document.createElement("div"); document.body.appendChild(host);
  const root = createRoot(host);
  const target = (address: string): Bookmark => ({ id: address, kind: "gwar", address, name: address, updatedAt: 1 });
  try {
    await act(async () => root.render(<Fixture />));
    await act(async () => open(target("other.example")));
    expect(connectInteractive).toHaveBeenCalledOnce();
    expect(useUi.getState().section).toBe("home");
    await act(async () => open(target("current.example")));
    expect(connectInteractive).toHaveBeenCalledOnce();
    expect(useUi.getState().section).toBe("servers");
  } finally {
    await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals();
  }
});
