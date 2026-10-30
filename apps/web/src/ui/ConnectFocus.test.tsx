// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useConnectUi, useUi } from "../state/stores";
import { ConnectScreen } from "./ConnectScreen";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

vi.mock("../lib/official", async get => ({ ...await get<typeof import("../lib/official")>(), CONNECT_URL: "http://127.0.0.1:18980" }));
afterEach(() => vi.unstubAllGlobals());

it.each([false, true])("a failed connection's remounted form cannot steal focus from the safe trust action (password=%s)", async needPassword => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("fetch", vi.fn(async () => new Response("not a server", { status: 404 })));
  useSettings.setState({ language: "en", lastAddress: "", lastKind: "gwar" });
  useConnectUi.setState({ otherTab: null, busy: false, error: null, needPassword });
  useUi.getState().openDialog({ kind: "confirm", title: "Server identity changed", body: "The saved key differs.",
    confirmLabel: "Trust this key", danger: true, serverIdentity: true, onConfirm: vi.fn() });
  const host = document.createElement("div"); document.body.appendChild(host); const root = createRoot(host);
  const tree = (form: boolean) => <TooltipProvider>{form && <ConnectScreen key="form" />}<Dialogs key="dialog" /></TooltipProvider>;
  try {
    await act(async () => root.render(tree(false)));
    expect(document.activeElement?.textContent).toBe("Cancel");
    await act(async () => root.render(tree(true)));
    expect(document.activeElement?.textContent).toBe("Cancel");
  } finally {
    await act(async () => root.unmount()); host.remove(); useUi.getState().closeDialog();
  }
});
