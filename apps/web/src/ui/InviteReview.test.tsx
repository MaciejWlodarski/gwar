// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useConnectUi, useUi } from "../state/stores";
import { InviteReviewDialog } from "./InviteReview";
const disconnect = vi.hoisted(() => vi.fn(async () => {}));
vi.mock("../state/controller", () => ({ controller: { disconnect } }));
it("keeps the current session until a click, then stages the invite without connecting", async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  const target = { server: "wss://room.example/ws", code: "abc", serverKey: "A".repeat(43) };
  useUi.getState().openDialog({ kind: "inviteReview", target });
  useConnectUi.setState({ invite: null });
  const host = document.createElement("div"); document.body.appendChild(host); const root = createRoot(host);
  try {
    await act(async () => root.render(<InviteReviewDialog target={target} />));
    expect(disconnect).not.toHaveBeenCalled();
    expect(useConnectUi.getState().invite).toBeNull();
    const review = [...document.querySelectorAll("button")].find(b => b.textContent === "Review invitation")!;
    await act(async () => review.click());
    expect(disconnect).toHaveBeenCalledOnce();
    expect(useConnectUi.getState().invite).toEqual(target);
    expect(useUi.getState()).toMatchObject({ dialog: { kind: "none" }, section: "servers" });
  } finally { await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals(); }
});
