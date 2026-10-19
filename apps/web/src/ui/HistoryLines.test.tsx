// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import { welcome } from "../net/testing";
import type { ChatItem } from "../state/types";
import type { Roster } from "../mls/types";
import { MessageList } from "./Chat";
import { TooltipProvider } from "./kit";

const live = "L".repeat(43), earlier = "E".repeat(43);
let roster: Roster | null = null;
vi.mock("./useVerifiedHandle", () => ({ useVerifiedHandle: () => null }));
vi.mock("../state/controller", () => ({ controller: { roster: () => roster, onRoster: () => () => {}, messageServerKey: "server", messageUid: Promise.resolve(undefined), loadOlder: () => {} }, UploadError: class extends Error {} }));

let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  const lobby = welcome().channels[0]!;
  useSession.setState({ kind: "gwar", viewChannel: 7, channels: { 7: { ...lobby, id: 7, encrypted: true, history: true } }, members: {} });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
const unopened = (id: number, group: string): ChatItem => ({ kind: "msg", key: `m${id}`, at: 1700000000000 + id, cannotOpen: "no_key",
  msg: { id, target: { channel: 7 }, author: 1, author_uid: "alice", author_name: "Alice", text: "", mentions: [], sent_at: 1700000000000 + id },
  sealed: { conversation: "channel:7", group, epoch: 1, message_id: "m", revision: 0, ciphertext: "" } as never });
const show = async (items: ChatItem[], value: Roster) => {
  roster = value;
  useSession.setState({ threads: { "ch:7": { items, unread: 0, mentions: 0, hasMore: false, loaded: true, lastRead: 0, locked: false } } });
  await act(async () => root.render(<TooltipProvider><MessageList threadKey="channel" /></TooltipProvider>));
};
const text = () => container.textContent ?? "";

it("says quietly that a conversation was restarted, and waits only for keys someone can still share", async () => {
  const restarted = { group: live, restarted: true, history: true, since: 0, members: [] };
  await show([unopened(1, earlier)], restarted);
  expect(text()).toContain("This conversation was restarted.");
  expect(text()).not.toContain("Earlier messages will appear");
  await show([unopened(1, earlier), unopened(2, live)], restarted);
  expect(text()).toContain("Earlier messages will appear");
  await show([unopened(2, live)], { ...restarted, restarted: false });
  expect(text()).not.toContain("restarted");
});
