// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from "vitest";
import { useConnectUi, useSession, useUi } from "./stores";
import { consumeInviteFromLocation, initDesktopInvites } from "./invite-boot";

const bridge = vi.hoisted(() => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("../platform", () => ({ isDesktop: () => true, tauri: async () => bridge }));
const key = "A".repeat(43);
const link = `gwar://invite/?server=wss://room.example/ws&invite=abc&server_key=${key}`;
let receive: (links: string[]) => void;
beforeEach(() => {
  bridge.invoke.mockReset().mockResolvedValue([]);
  bridge.listen.mockReset().mockImplementation(async (_event, callback) => { receive = callback; return () => {}; });
  useConnectUi.setState({ invite: null, inviteLaunch: false, busy: false });
  useSession.setState({ phase: "idle" });
  useUi.getState().closeDialog();
});

it("stages a cold invite before passive restoration can begin, with no connect command", async () => {
  bridge.invoke.mockResolvedValue([link]);
  await initDesktopInvites();
  expect(useConnectUi.getState()).toMatchObject({ inviteLaunch: true, invite: { code: "abc", server: "wss://room.example/ws" } });
  expect(bridge.listen.mock.invocationCallOrder[0]).toBeLessThan(bridge.invoke.mock.invocationCallOrder[0]!);
  expect(bridge.invoke).toHaveBeenCalledExactlyOnceWith("desktop_invite_links");
});
it("an invalid cold link also blocks automatic connection", async () => {
  bridge.invoke.mockResolvedValue(["gwar://execute/join"]);
  await initDesktopInvites();
  expect(useConnectUi.getState()).toMatchObject({ inviteLaunch: true, invite: null });
});
it("a link received while online or connecting only opens a review dialog", async () => {
  await initDesktopInvites();
  useSession.setState({ phase: "online" });
  receive([link]);
  expect(useSession.getState().phase).toBe("online");
  expect(useConnectUi.getState().invite).toBeNull();
  expect(useUi.getState().dialog).toMatchObject({ kind: "inviteReview", target: { code: "abc" } });
  useUi.getState().closeDialog();
  receive([link]);
  expect(useUi.getState().dialog.kind).toBe("inviteReview"); // A later click on the same link still works.
});
it("keeps malformed web invites inert and removes their query secrets", () => {
  const replace = vi.spyOn(window.history, "replaceState");
  consumeInviteFromLocation({ search: "?invite=abc&server_key=bad&other=kept", pathname: "/", hash: "" });
  expect(useConnectUi.getState()).toMatchObject({ invite: null, inviteLaunch: true });
  expect(replace).toHaveBeenCalledWith(null, "", "/?other=kept");
});

it("a live invitation wins over the startup read without making later clicks inert", async () => {
  bridge.invoke.mockImplementationOnce(async () => { receive([link.replace("invite=abc", "invite=new")]); return [link]; });
  await initDesktopInvites();
  expect(useConnectUi.getState().invite?.code).toBe("new");
  receive([link]);
  expect(useConnectUi.getState().invite?.code).toBe("abc");
});
