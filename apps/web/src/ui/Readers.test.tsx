// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession } from "../state/stores";
import { welcome } from "../net/testing";
import { ReadersDialog } from "./Readers";
import { TooltipProvider } from "./kit";

const roster = { group: "g", restarted: false, history: true, since: 0, members: [{ uid: "alice", device_key: "a1" }, { uid: "alice", device_key: "a2" }, { uid: "bob", device_key: "b1" }] };
vi.mock("../state/controller", () => ({ controller: { roster: () => roster, onRoster: () => () => {} }, UploadError: class extends Error {} }));

let container: HTMLDivElement, root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  useSettings.setState({ language: "en" });
  const lobby = welcome().channels[0]!;
  useSession.setState({ kind: "gwar", channels: { 7: { ...lobby, id: 7, encrypted: true, history: true } }, members: {} });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.unstubAllGlobals(); });
const securing = (value: boolean) => act(async () => useSession.setState(s => ({ threads: { ...s.threads,
  "ch:7": { items: [], unread: 0, mentions: 0, hasMore: false, loaded: true, lastRead: 0, locked: false, securing: value } } })));

it("lists the group's devices per member and says when the view may be out of date", async () => {
  await securing(false);
  await act(async () => root.render(<TooltipProvider><ReadersDialog thread="ch:7" /></TooltipProvider>));
  const text = () => document.body.textContent ?? "";
  expect(text()).toContain("as this device sees it");
  expect(text()).toContain("2 devices");
  expect(text()).toContain("1 device");
  await securing(true);
  expect(text()).toContain("still being secured");
  expect(text()).not.toContain("as this device sees it");
  // The last known group stays listed meanwhile.
  expect(text()).toContain("2 devices");
  await securing(false);
  expect(text()).toContain("as this device sees it");
});

it.each([
  ["en", "Devices in this conversation's group, as this device sees it.", "This conversation is still being secured on this device."],
  ["pl", "Urządzenia w grupie tej rozmowy, tak jak widzi ją to urządzenie.", "Ta rozmowa jest jeszcze zabezpieczana na tym urządzeniu."],
] as const)("uses conversation wording for server chat in %s", async (language, hint, pending) => {
  useSettings.setState({ language });
  useSession.setState({ server: { ...welcome().server, server_chat: { encrypted: true, history: true } } });
  const state = (value: boolean) => act(async () => useSession.setState({ threads: {
    server: { items: [], unread: 0, mentions: 0, hasMore: false, loaded: true, lastRead: 0, locked: false, securing: value },
  } }));
  await state(false);
  await act(async () => root.render(<TooltipProvider><ReadersDialog thread="server" /></TooltipProvider>));
  expect(document.body.textContent).toContain(hint);
  expect(document.body.textContent).not.toContain(pending);
  await state(true);
  expect(document.body.textContent).toContain(pending);
  expect(document.body.textContent).not.toContain(hint);
});
