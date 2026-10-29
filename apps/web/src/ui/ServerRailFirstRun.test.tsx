// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useAccount } from "../state/account";
import { useSettings } from "../state/settings";
import { ServerRail } from "./ServerRail";
import { TooltipProvider } from "./kit";

let root: Root, container: HTMLDivElement;
beforeEach(async () => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useAccount.setState({ account: null });
  useSettings.setState({ language: "en", bookmarks: [], serverGroups: [], onboarding: { completed: false, firstUsedAt: 1, nudgeDismissed: false, roleReceived: false } });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  await act(async () => root.render(<TooltipProvider><ServerRail /></TooltipProvider>));
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
it("offers Add server only after choosing a name or explicitly skipping the name step", async () => {
  expect(container.querySelector('[aria-label="Add a server"]')).toBeNull();
  await act(async () => useSettings.getState().setOnboarding({ completed: true }));
  expect(container.querySelector('[aria-label="Add a server"]')).not.toBeNull();
});
