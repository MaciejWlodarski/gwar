// @vitest-environment happy-dom
import "fake-indexeddb/auto";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { controller } from "../state/controller";
import { initialState } from "../state/reducer";
import { useSession, useUi } from "../state/stores";
import { useSettings } from "../state/settings";
import { welcome } from "../net/testing";
import { ChatArea } from "./Chat";
import { TooltipProvider } from "./kit";

vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));
const request = vi.fn(async () => ({})), send = vi.fn(async () => {});
const roster = { group: "g", history: true, restarted: true, since: 0, members: [] };
const internals = controller as unknown as { conn: unknown; sealed: unknown; runtime: unknown };
let root: Root, container: HTMLDivElement;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  request.mockClear(); send.mockClear();
  useSettings.setState({ language: "en" });
  useUi.setState({ editing: null, dialog: { kind: "none" } });
  useSession.setState({ ...initialState, kind: "vc", phase: "online", activeThread: "server", me: { uid: "me", session: 1 },
    server: { ...welcome().server, upload_limit: 1000, server_chat: { encrypted: true, history: true } }, permissions: ["file_upload", "message_manage"],
    threads: { server: { items: [], unread: 0, mentions: 0, hasMore: true, loaded: true, lastRead: 0, locked: false, securing: false } } });
  internals.conn = { request, serverKey: "", isOnline: true };
  internals.sealed = { send };
  internals.runtime = { roster: () => roster };
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  internals.conn = null; internals.sealed = null; internals.runtime = null;
  vi.restoreAllMocks(); vi.unstubAllGlobals();
});
const show = () => act(async () => root.render(<TooltipProvider><ChatArea /></TooltipProvider>));
const button = (label: string) => container.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`);

it.each(["encrypted", "plaintext", "teamspeak"] as const)("uses the %s server sending and attachment path", async mode => {
  const encrypted = mode === "encrypted";
  useSession.setState({ kind: mode === "teamspeak" ? "teamspeak" : "vc", server: { ...welcome().server, upload_limit: 1000, server_chat: { encrypted, history: encrypted } } });
  await show();
  expect(!!button("Who can read")).toBe(encrypted);
  expect(container.textContent).toContain(encrypted ? "Messages here go to everyone on the server and are saved." : "Messages here go to everyone on the server and are not saved.");
  expect(!!container.querySelector('input[type="file"]')).toBe(encrypted);
  expect(container.textContent?.includes("This conversation was restarted.")).toBe(encrypted);
  if (encrypted) {
    await act(async () => button("Who can read")!.click());
    expect(useUi.getState().dialog).toEqual({ kind: "readers", thread: "server" });
  }
  const input = container.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input, "server hello");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
  if (encrypted) {
    expect(send).toHaveBeenCalledWith("server", "server hello", [], undefined, []);
    expect(request).not.toHaveBeenCalled();
  } else {
    expect(send).not.toHaveBeenCalled();
    expect(request).toHaveBeenCalledWith("chat.send", expect.objectContaining({ target: "server", text: "server hello" }));
  }
});

it("loads older sealed server rows and offers moderator removal", async () => {
  useSession.setState(s => ({ threads: { server: { ...s.threads.server!, items: [{ kind: "msg", key: "e1", at: 1,
    msg: { id: 1, target: "server", author: 2, author_uid: "other", author_name: "Other", text: "hello", sent_at: 1 },
    sealed: { id: 1, conversation: "server", group: "g", epoch: 0, author_uid: "other", author_name: "Other", message_id: "m", revision: 0, ciphertext: "x", sent_at: 1, deleted: false } }] } } }));
  const load = vi.spyOn(controller, "loadOlder").mockResolvedValue();
  await show();
  expect(button("Delete message")).not.toBeNull();
  await act(async () => container.querySelector('[role="log"]')!.dispatchEvent(new Event("scroll")));
  expect(load).toHaveBeenCalledOnce();
});


it("keeps the server composer disabled until the group is ready", async () => {
  useSession.setState(s => ({ threads: { server: { ...s.threads.server!, securing: true } } }));
  await show();
  expect(container.querySelector("textarea")?.disabled).toBe(true);
  expect(container.querySelector("textarea")?.placeholder).toBe("Securing this conversation…");
});


it("edits the latest own sealed server message from the empty composer", async () => {
  useSession.setState(s => ({ threads: { server: { ...s.threads.server!, items: [{ kind: "msg", key: "e2", at: 2,
    msg: { id: 2, target: "server", author: 1, author_uid: "me", author_name: "Me", text: "own", sent_at: 2 },
    sealed: { id: 2, conversation: "server", group: "g", epoch: 0, author_uid: "me", author_name: "Me", message_id: "m", revision: 0, ciphertext: "x", sent_at: 2, deleted: false } }] } } }));
  await show();
  await act(async () => container.querySelector("textarea")!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true })));
  expect(useUi.getState().editing).toBe(2);
});
