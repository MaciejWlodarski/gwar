// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { buildInviteLink } from "../net/invite";
import { ConnectScreen } from "./ConnectScreen";
import { useConnectUi, useSession } from "../state/stores";
import { useSettings } from "../state/settings";
import { useAccount } from "../state/account";

const connect = vi.hoisted(() => vi.fn(async () => true));
vi.mock("../state/controller", () => ({ controller: { connectInteractive: connect }, bookmarkParams: vi.fn(), describeBan: vi.fn() }));
vi.mock("./hooks", () => ({ useTsIdentities: () => null }));
let host: HTMLDivElement, root: ReturnType<typeof createRoot>;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en", lastAddress: "previous.example", lastKind: "gwar", onboarding: null, bookmarks: [] });
  useAccount.setState({ loaded: true, account: null });
  useSession.setState({ closeReason: null });
  useConnectUi.setState({ invite: null, busy: false, error: "Previous server error", needPassword: true, serverName: "Previous server", otherTab: null });
  connect.mockClear();
  host = document.createElement("div"); document.body.appendChild(host); root = createRoot(host);
});
afterEach(async () => { await act(async () => root.unmount()); host.remove(); vi.unstubAllGlobals(); });

it.each(["warm", "pasted"])("S2: a %s invitation clears the previous server password and prompts before Join", async source => {
  await act(async () => root.render(<ConnectScreen />));
  const input = host.querySelector<HTMLInputElement>('input[type="password"]')!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, "previous-server-secret");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(connect).toHaveBeenCalledWith(expect.objectContaining({ password: "previous-server-secret" }), { remember: true });
  connect.mockClear();
  const target = { code: "abc", server: "wss://invited.example/ws", serverKey: "A".repeat(43), name: "Invited server" };
  await act(async () => {
    if (source === "warm") useConnectUi.getState().set({ invite: target });
    else {
      const address = host.querySelector<HTMLInputElement>('input[name="server-address"]')!;
      const pasted = buildInviteLink({ webOrigin: "https://web.example", serverEndpoint: target.server, ...target });
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(address, pasted);
      address.dispatchEvent(new Event("input", { bubbles: true }));
    }
  });
  expect(useConnectUi.getState()).toMatchObject({ needPassword: false, serverName: null, error: null });
  expect(host.querySelector('input[type="password"]')).toBeNull();
  await act(async () => host.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(connect).toHaveBeenCalledWith(expect.objectContaining({ address: "invited.example", password: "", invite: "abc" }), { remember: true });
});
