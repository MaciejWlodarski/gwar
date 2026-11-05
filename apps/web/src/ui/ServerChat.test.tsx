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
import { useSession, useUi, useOutbox } from "../state/stores";
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
  useOutbox.getState().clear();
  vi.spyOn(MessageVerificationCache.prototype, "verify").mockImplementation(async msg => ({
    status: msg.text === "unverified" ? "invalid" : "verified", continuity: "proven",
    authorization: msg.text === "revoked" ? ["device-revoked"] : [],
  }));
  useSettings.setState({ language: "en" });
  useUi.setState({ editing: null, dialog: { kind: "none" } });
  useSession.setState({ ...initialState, kind: "gwar", phase: "online", activeThread: "server", me: { uid: "me", session: 1 },
    server: { ...welcome().server, upload_limit: 1000, server_chat: { encrypted: true, history: true } }, permissions: ["attach_files", "manage_messages", "send_messages"],
    threads: { server: { items: [], unread: 0, mentions: 0, hasMore: true, loaded: true, lastRead: 0, locked: false, securing: false } } });
  internals.conn = { request, serverKey: "", isOnline: true };
  internals.sealed = { send, delivered: () => true };
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


it.each([false, true])("does not offer header voice joining for a TeamSpeak spacer (mobile=%s)", async mobile => {
  vi.spyOn(window, "matchMedia").mockImplementation(query => ({ matches: mobile, media: query, addEventListener: vi.fn(), removeEventListener: vi.fn() }) as unknown as MediaQueryList);
  const join = vi.spyOn(controller, "joinChannelInteractive").mockResolvedValue();
  const channel = { id: 2, parent: null, name: "[cspacer1]Welcome", topic: "", position: 2, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false };
  useSession.setState({ kind: "teamspeak", activeThread: "channel", viewChannel: 2, permissions: ["connect"], channels: { 2: channel },
    owner: { uid: "me" } as never,
    clients: { 1: { id: 1, uid: "me", nickname: "Me", channel: null, groups: [], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: false, voice_binding: null } } });
  await show();
  const action = () => [...container.querySelectorAll<HTMLButtonElement>("header button")].find(el => el.textContent === "Join voice");
  expect(action()).toBeUndefined();
  expect(join).not.toHaveBeenCalled();
  // Gwar does not interpret spacer syntax, and nested TeamSpeak names are real channels.
  await act(async () => useSession.setState({ kind: "gwar" }));
  expect(action()).toBeDefined();
  await act(async () => action()!.click());
  expect(join).toHaveBeenCalledWith(2);
  await act(async () => useSession.setState({ kind: "teamspeak", channels: { 1: { ...channel, id: 1, name: "Lobby" }, 2: { ...channel, parent: 1 } } }));
  expect(action()).toBeDefined();
  await act(async () => action()!.click());
  expect(join).toHaveBeenCalledTimes(2);
  // The tree promotes an orphan to its top level, where spacer syntax applies again.
  await act(async () => useSession.setState({ channels: { 2: { ...channel, parent: 1 } } }));
  expect(action()).toBeUndefined();
});

it.each(["encrypted", "plaintext", "teamspeak"] as const)("uses the %s server sending and attachment path", async mode => {
  const encrypted = mode === "encrypted";
  useSession.setState({ kind: mode === "teamspeak" ? "teamspeak" : "gwar", server: { ...welcome().server, upload_limit: 1000, server_chat: { encrypted, history: encrypted } } });
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
    expect(send).toHaveBeenCalledWith("server", "server hello", [], undefined, [], { message_id: expect.any(String), created_at: expect.any(Number) }, expect.any(Function));
    expect(request).not.toHaveBeenCalled();
  } else {
    expect(send).not.toHaveBeenCalled();
    expect(request).toHaveBeenCalledWith("chat.send", expect.objectContaining({ target: "server", text: "server hello" }));
  }
});

it("keeps the composer text when a send is refused before admission", async () => {
  vi.spyOn(controller, "sendChat").mockResolvedValue(false);
  await show();
  const input = container.querySelector("textarea")!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input, "keep this draft");
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
  expect(input.value).toBe("keep this draft");
});

it("clears only the original retained draft when a later admission succeeds", async () => {
  let admit!: () => void;
  vi.spyOn(controller, "sendChat").mockImplementation(async (_key, _text, options) => { admit = options!.onRetained!; return false; });
  await show();
  const input = container.querySelector("textarea")!;
  const type = async (value: string) => act(async () => {
    Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await type("original draft");
  await act(async () => input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true })));
  expect(input.value).toBe("original draft");
  await type("new edits");
  await act(async () => admit());
  expect(input.value).toBe("new edits");
});

it("shows a failed encrypted message in its timeline position with Polish retry labels", async () => {
  useSettings.setState({ language: "pl" });
  useOutbox.getState().add({ thread: "server", text: "failed earlier", at: 100, session: controller.sendSession, sealed: { message_id: "retry", created_at: 100 }, serverKey: "" });
  useSession.setState(s => ({ threads: { server: { ...s.threads.server!, items: [{ kind: "msg", key: "e2", at: 200,
    msg: { id: 2, target: "server", author: 2, author_uid: "other", author_name: "Other", text: "later message", sent_at: 200 } }] } } }));
  await show();
  const failed = container.querySelector('[role="alert"]')!;
  const later = container.querySelector('[data-message]')!;
  expect(failed.textContent).toContain("Nie wysłano");
  expect(failed.textContent).toContain("Wyślij ponownie");
  expect(failed.textContent).toContain("Odrzuć");
  expect(failed.compareDocumentPosition(later) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
});

it("hides a retained in-flight attempt until delivery fails", async () => {
  const item = useOutbox.getState().add({ thread: "server", text: "waiting for delivery", serverKey: "", sending: true,
    sealed: { message_id: "pending-id", created_at: 100 }, session: controller.sendSession });
  await show();
  expect(container.querySelector('[role="alert"]')).toBeNull();
  await act(async () => useOutbox.getState().update(item.id, { sending: false }));
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("Not sent");
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("Send again");
});

it("keeps attachment chips and requires an explicit fresh send after the session changes", async () => {
  const retry = vi.spyOn(controller, "retrySend").mockResolvedValue();
  useOutbox.getState().add({ thread: "server", text: "failed draft", serverKey: "", authorUid: "me",
    attachments: ["f1"], sealed: { message_id: "old-id", created_at: 100 },
    attachmentMetadata: [{ id: "f1", name: "notes.txt", mime: "text/plain", size: 5, url: "/files/f1/file", sha256: "hash", sealed: { key: "key", sha256: "cipherhash", size: 33 } }] });
  await show();
  const failed = container.querySelector('[role="alert"]')!;
  expect(failed.firstElementChild?.nextElementSibling?.firstElementChild?.textContent?.trim()).toBe("Not sent");
  expect(failed.textContent).not.toMatch(/storage|encrypted|session|re-upload/i);
  expect(failed.textContent).toContain("notes.txt");
  expect(failed.querySelector('[data-attachment="file"]')?.textContent).toContain("notes.txt");
  expect(failed.querySelector("[data-missing]")).toBeNull();
  const fresh = [...failed.querySelectorAll("button")].find(b => b.textContent?.includes("Send again"))!;
  expect(fresh.disabled).toBe(false);
  await act(async () => fresh.click());
  expect(retry).toHaveBeenCalledWith(useOutbox.getState().failed[0]!.id, true);
});

it("says on one line why a message was not sent, and shows a retry in progress", async () => {
  const item = useOutbox.getState().add({ thread: "server", text: "slow server", serverKey: "", authorUid: "me", reason: "err.req.timeout",
    sealed: { message_id: "slow-id", created_at: 100 }, session: controller.sendSession });
  await show();
  const failed = container.querySelector('[role="alert"]')!;
  const status = failed.querySelector("[data-failed-status]")!;
  expect(status.textContent).toBe("Not sent · The server took too long to answer.");
  // One line from sm up; a narrow (touch) screen wraps it instead of hiding the rest behind a title.
  expect(status.classList.contains("sm:truncate")).toBe(true);
  expect(status.classList.contains("truncate")).toBe(false);
  await act(async () => useOutbox.getState().update(item.id, { retrying: true }));
  const sending = [...failed.querySelectorAll("button")].find(b => b.textContent?.includes("Sending…"))!;
  expect(sending.disabled).toBe(true);
  expect(failed.getAttribute("aria-busy")).toBe("true");
  await act(async () => useOutbox.getState().update(item.id, { retrying: false }));
  expect([...failed.querySelectorAll("button")].find(b => b.textContent?.includes("Send again"))!.disabled).toBe(false);
});

it("says why in the language chosen now, in words that stay true after a reconnect", async () => {
  useOutbox.getState().add({ thread: "server", text: "while offline", serverKey: "", authorUid: "me", reason: "chat.failed.lost",
    sealed: { message_id: "lost-id", created_at: 100 }, session: controller.sendSession });
  await show();
  const status = () => container.querySelector("[data-failed-status]")!.textContent;
  expect(status()).toBe("Not sent · The connection was lost.");
  await act(async () => useSettings.setState({ language: "pl" }));
  expect(status()).toBe("Nie wysłano · Połączenie zostało przerwane.");
});

it("offers only Dismiss for a message that may have been sent or removed", async () => {
  useOutbox.getState().add({ thread: "server", text: "gone", serverKey: "", authorUid: "me", unknown: true,
    sealed: { message_id: "gone-id", created_at: 100 }, session: controller.sendSession });
  await show();
  const failed = container.querySelector('[role="alert"]')!;
  expect(failed.querySelector("[data-failed-status]")!.textContent).toBe("May have been sent or removed");
  expect([...failed.querySelectorAll("button")].map(b => b.textContent)).toEqual(["Dismiss"]);
});

it("keeps a failed message visible when an attachment cannot be sent", async () => {
  useOutbox.getState().add({ thread: "server", text: "text survives", serverKey: "",
    attachments: ["lost-file"], sealed: { message_id: "old-id", created_at: 100 } });
  await show();
  const failed = container.querySelector('[role="alert"]')!;
  expect(failed.textContent).toContain("text survives");
  expect(failed.textContent).not.toMatch(/keys|storage|session/i);
  expect(failed.querySelector('[data-attachment="file"][data-missing="true"]')?.textContent).toBe("Attachment");
  const missing = failed.querySelector<HTMLElement>('[data-missing="true"]')!;
  expect(missing.classList.contains("opacity-50")).toBe(true);
  await act(async () => missing.dispatchEvent(new FocusEvent("focusin", { bubbles: true })));
  expect(document.querySelector('[role="tooltip"]')?.textContent).toBe("Missing");
  expect(failed.querySelector("button")!.disabled).toBe(true);
  expect([...failed.querySelectorAll("button")].find(b => b.textContent === "Dismiss")!.disabled).toBe(false);
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


it("writes only in my own channel on TeamSpeak, which gives a channel's chat only to the people in it", async () => {
  const channel = (id: number) => ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false });
  const me = { id: 1, uid: "me", nickname: "me", channel: 1, groups: [], platform: "ts3" as const, muted: false, deafened: false, away: null, talking: false, voice: true, voice_binding: null };
  const thread = { items: [], unread: 0, mentions: 0, hasMore: false, loaded: true, lastRead: 0, locked: false };
  useSession.setState({ kind: "teamspeak", activeThread: "channel", viewChannel: 2, channels: { 1: channel(1), 2: channel(2) }, clients: { 1: me },
    permissions: ["send_messages"], server: { ...welcome().server, teamspeak: true }, threads: { "ch:1": thread, "ch:2": thread } });
  await show();
  const input = () => container.querySelector("textarea")!;
  expect(input().disabled).toBe(true);
  expect(input().placeholder).toBe("Join this channel to write here");
  expect(container.textContent).toContain("Join this channel to read and write here.");
  await act(async () => useSession.setState({ viewChannel: 1 }));
  expect(input().disabled).toBe(false);
  expect(input().placeholder).toBe("Message #c1");
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
