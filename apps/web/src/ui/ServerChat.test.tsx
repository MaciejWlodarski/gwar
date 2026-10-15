// @vitest-environment happy-dom
import "fake-indexeddb/auto";
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { DELETION_TEXT } from "../net/message-signature";
import { MessageVerificationCache } from "../net/message-verification-cache";
import type { ChatItem } from "../state/types";
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
  vi.spyOn(MessageVerificationCache.prototype, "verify").mockImplementation(async msg => ({
    status: msg.text === "unverified" ? "invalid" : "verified", continuity: "proven",
    authorization: msg.text === "revoked" ? ["device-revoked"] : [],
  }));
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
  await waitForEditButton();
  await arrowUp();
  expect(useUi.getState().editing).toBe(2);
});

const arrowUp = () => act(async () => container.querySelector("textarea")!.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true })));
const waitForEditButton = () => vi.waitFor(async () => {
  await act(async () => { await new Promise(resolve => setTimeout(resolve, 20)); });
  expect(button("Edit message")).not.toBeNull();
});
const ownRow = (id: number, extra: Partial<Extract<ChatItem, { kind: "msg" }>> = {}): Extract<ChatItem, { kind: "msg" }> => ({
  kind: "msg", key: `e${id}`, at: id,
  msg: { id, target: "server", author: 1, author_uid: "me", author_name: "Me", text: "editable", sent_at: id },
  sealed: { id, conversation: "server", group: "g", epoch: 0, author_uid: "me", author_name: "Me", message_id: `m${id}`, revision: 0, ciphertext: "x", sent_at: id, deleted: false },
  ...extra,
});

it.each(["deleted", "removed", "no_key", "cannot_open", "unverified", "revoked", "pending"] as const)("ArrowUp skips the latest %s server row", async reason => {
  let latest = ownRow(2);
  if (reason === "deleted") latest = { ...latest, msg: { ...latest.msg, text: DELETION_TEXT }, sealed: { ...latest.sealed!, deleted: true } };
  else if (reason === "removed") latest.removed = true;
  else if (reason === "no_key" || reason === "cannot_open") latest = { ...latest, cannotOpen: reason, msg: { ...latest.msg, text: "" } };
  else latest.msg = { ...latest.msg, text: reason };
  if (reason === "pending") vi.mocked(MessageVerificationCache.prototype.verify).mockImplementation(async msg => msg.id === 2
    ? new Promise(() => {}) : { status: "verified", continuity: "proven", authorization: [] });
  useSession.setState(s => ({ threads: { server: { ...s.threads.server!, items: [ownRow(1), latest] } } }));
  await show(); await waitForEditButton();
  expect(container.querySelector('[data-message="2"] button[aria-label="Edit message"]')).toBeNull();
  await arrowUp();
  expect(useUi.getState().editing).toBe(1);
});

it.each([false, true])("ArrowUp leaves live-only plaintext server rows alone (encrypted=%s)", async encrypted => {
  useSession.setState(s => ({ server: { ...s.server!, server_chat: { encrypted, history: encrypted } },
    threads: { server: { ...s.threads.server!, items: [ownRow(1, { sealed: undefined })] } } }));
  await show();
  await vi.waitFor(async () => {
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 20)); });
    expect(container.querySelector('[data-message-verification="verified"]')).not.toBeNull();
  });
  expect(button("Edit message")).toBeNull();
  await arrowUp();
  expect(useUi.getState().editing).toBeNull();
});
