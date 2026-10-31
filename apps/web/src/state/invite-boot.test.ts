// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from "vitest";
import { useConnectUi, useSession, useUi } from "./stores";
import { consumeInviteFromLocation, initDesktopInvites } from "./invite-boot";

const bridge = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("../platform", () => ({ isDesktop: () => true, tauri: async () => bridge }));
const key = "A".repeat(43);
const link = `gwar://invite/?server=wss://room.example/ws&invite=abc&server_key=${key}`;
let receive: () => Promise<void>;
const launch = (links: string[], launched = links.length > 0) => ({ launched, links });
beforeEach(() => {
  bridge.invoke.mockReset().mockResolvedValue(launch([]));
  bridge.listen.mockReset().mockImplementation(async (_event, callback) => { receive = callback; return () => {}; });
  useConnectUi.setState({ invite: null, inviteLaunch: false, busy: false });
  useSession.setState({ phase: "idle" });
  useUi.getState().closeDialog();
});

it("stages a cold invite before passive restoration can begin, with no connect command", async () => {
  bridge.invoke.mockResolvedValue(launch([link]));
  await initDesktopInvites();
  expect(useConnectUi.getState()).toMatchObject({ inviteLaunch: true, invite: { code: "abc", server: "wss://room.example/ws" } });
  expect(bridge.listen.mock.invocationCallOrder[0]).toBeLessThan(bridge.invoke.mock.invocationCallOrder[0]!);
  expect(bridge.invoke).toHaveBeenCalledExactlyOnceWith("desktop_invite_links");
});
it("an invalid cold link also blocks automatic connection", async () => {
  bridge.invoke.mockResolvedValue(launch(["gwar://execute/join"]));
  await initDesktopInvites();
  expect(useConnectUi.getState()).toMatchObject({ inviteLaunch: true, invite: null });
});
it("a link received while online or connecting only opens a review dialog", async () => {
  await initDesktopInvites();
  useSession.setState({ phase: "online" });
  bridge.invoke.mockResolvedValueOnce(launch([link]));
  await receive();
  expect(useSession.getState().phase).toBe("online");
  expect(useConnectUi.getState().invite).toBeNull();
  expect(useUi.getState().dialog).toMatchObject({ kind: "inviteReview", target: { code: "abc" } });
  useUi.getState().closeDialog();
  bridge.invoke.mockResolvedValueOnce(launch([link]));
  await receive();
  expect(useUi.getState().dialog.kind).toBe("inviteReview"); // A later click on the same link still works.
});
it("keeps malformed web invites inert and removes their query secrets", () => {
  const replace = vi.spyOn(window.history, "replaceState");
  consumeInviteFromLocation({ search: "?invite=abc&server_key=bad&other=kept", pathname: "/", hash: "" });
  expect(useConnectUi.getState()).toMatchObject({ invite: null, inviteLaunch: true });
  expect(replace).toHaveBeenCalledWith(null, "", "/?other=kept");
});

it("drains a queued wake before restoration and preserves later clicks on the same link", async () => {
  bridge.invoke.mockImplementationOnce(async () => {
    bridge.invoke.mockResolvedValueOnce(launch([link.replace("invite=abc", "invite=new")]));
    void receive();
    return launch([link]);
  });
  await initDesktopInvites();
  expect(useConnectUi.getState().invite?.code).toBe("new");
  expect(bridge.invoke).toHaveBeenCalledTimes(2);
  bridge.invoke.mockResolvedValueOnce(launch([link]));
  await receive();
  expect(useConnectUi.getState().invite?.code).toBe("abc");
});

it("S1: malformed cold launch intent suppresses restoration even when no URL parsed", async () => {
  bridge.invoke.mockResolvedValue({ launched: true, links: [] });
  await initDesktopInvites();
  expect(useConnectUi.getState()).toMatchObject({ inviteLaunch: true, invite: null });
});
