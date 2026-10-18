// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { Dialogs } from "./Dialogs";
import { TooltipProvider } from "./kit";

const mock = vi.hoisted(() => ({ create: vi.fn(async (_: object, _members?: string[]) => ({ id: 9 })), update: vi.fn(async (_: object) => ({})), ownerView: null as unknown }));
vi.mock("../state/controller", () => ({ controller: { messageServerKey: "server-a", createChannel: mock.create, updateChannel: mock.update, onRoster: () => () => {}, ownerView: () => mock.ownerView, privateMembers: () => null }, describeRequestError: String, UploadError: class extends Error {} }));

describe("creating a channel", () => {
  let container: HTMLDivElement, root: Root;
  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    mock.create.mockClear();
    mock.update.mockClear();
    useSettings.setState({ language: "en", channelEncryption: {} });
    useSession.setState({ kind: "vc", channels: {} });
    container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  });
  afterEach(async () => {
    await act(async () => root.unmount()); container.remove(); useUi.getState().closeDialog(); vi.unstubAllGlobals();
  });
  const open = async () => {
    useUi.getState().openDialog({ kind: "channelEdit", mode: "create", parent: null });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
  };
  const switchFor = (label: string) => [...document.querySelectorAll<HTMLButtonElement>('[role="switch"]')].find(s => s.id && document.querySelector(`label[for="${s.id}"]`)?.textContent?.startsWith(label));
  const password = () => document.querySelector('[role="dialog"] input[type="password"]');
  const create = async (name: string) => {
    const input = document.querySelector<HTMLInputElement>('[role="dialog"] input:not([type="password"])')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, name);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Create")!;
    await act(async () => button.click());
  };

  it("starts plaintext, drops the password when encrypted, and remembers the choice per server", async () => {
    await open();
    expect(switchFor("End-to-end encrypted")?.getAttribute("aria-checked")).toBe("false");
    expect(switchFor("New members see earlier messages")).toBeUndefined();
    expect(password()).not.toBeNull();
    await act(async () => switchFor("End-to-end encrypted")!.click());
    expect(password()).toBeNull();
    await act(async () => switchFor("New members see earlier messages")!.click());
    await create("secret");
    expect(mock.create).toHaveBeenCalledWith(expect.objectContaining({ name: "secret", password: null, encrypted: true, history: false }));
    expect(useSettings.getState().channelEncryption).toEqual({ "server-a": true });
    await open();
    expect(switchFor("End-to-end encrypted")?.getAttribute("aria-checked")).toBe("true");
  });
  it("sends no encryption options for a plaintext channel", async () => {
    await open();
    await create("open");
    expect(mock.create.mock.calls[0]![0]).not.toHaveProperty("encrypted");
    expect(useSettings.getState().channelEncryption).toEqual({ "server-a": false });
  });
  it("offers no encryption or private channels on TeamSpeak servers", async () => {
    useSession.setState({ kind: "teamspeak" });
    await open();
    expect(switchFor("End-to-end encrypted")).toBeUndefined();
    expect(switchFor("Private")).toBeUndefined();
    expect(password()).not.toBeNull();
  });
  it("creates a private channel admitting the creator, without password or encryption", async () => {
    useSession.setState({ me: { session: 1, uid: "me-uid" } as never, groups: {}, members: {} });
    await open();
    await act(async () => switchFor("Private")!.click());
    expect(password()).toBeNull();
    expect(switchFor("End-to-end encrypted")?.getAttribute("aria-checked")).toBe("false");
    expect(document.body.textContent).toContain("Who can see this channel");
    await create("hidden");
    expect(mock.create).toHaveBeenCalledWith(expect.objectContaining({ name: "hidden", password: null, private: true, access: { roles: [], users: ["me-uid"] } }));
    expect(mock.create.mock.calls[0]![0]).not.toHaveProperty("encrypted");
  });
  it("creates an encrypted private channel with a signed member list, for an authority only", async () => {
    useSession.setState({ me: { session: 1, uid: "me-uid" } as never, groups: {}, members: {} });
    mock.ownerView = null;
    await open();
    await act(async () => switchFor("Private")!.click());
    await act(async () => switchFor("End-to-end encrypted")!.click());
    expect(document.body.textContent).toContain("Only the server's owner, confirmers and managers");
    const button = () => [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Create")!;
    expect(button().disabled).toBe(true);
    await act(async () => useUi.getState().closeDialog());
    mock.ownerView = { owner: "me-uid", confirmers: [], managers: [], anchor_seq: 1 };
    await open();
    await act(async () => switchFor("Private")!.click());
    await act(async () => switchFor("End-to-end encrypted")!.click());
    await create("sealed");
    const [payload, members] = mock.create.mock.calls[0]!;
    expect(payload).toMatchObject({ name: "sealed", private: true, encrypted: true });
    expect(payload).not.toHaveProperty("access");
    expect(members).toEqual(["me-uid"]);
    mock.ownerView = null;
  });
  it("offers only roles I hold and no TeamSpeak users in the access picker", async () => {
    const role = (id: number, name: string) => ({ id, name, permissions: [], color: null });
    useSession.setState({
      me: { session: 1, uid: "me-uid" } as never,
      clients: { 1: { id: 1, uid: "me-uid", groups: [2, 5] } as never },
      groups: { 1: role(1, "Admin"), 2: role(2, "Member"), 5: role(5, "Crew"), 6: role(6, "Elsewhere") },
      members: { "ts:abc": { uid: "ts:abc", nickname: "Bridged", groups: [] } as never, alice: { uid: "alice", nickname: "Alice", groups: [] } as never },
    });
    await open();
    await act(async () => switchFor("Private")!.click());
    const text = document.querySelector('[role="dialog"]')!.textContent!;
    expect(text).toContain("Crew");
    expect(text).not.toContain("Elsewhere");
    expect(text).toContain("Alice");
    expect(text).not.toContain("Bridged");
  });
  it("saving other settings never resends an access list changed meanwhile", async () => {
    const channel = { id: 4, parent: null, name: "p", topic: "", position: 0, has_password: false, max_clients: null, encrypted: false, history: false, private: true, access: { roles: [], users: ["me-uid", "bob"] } };
    useSession.setState({ me: { session: 1, uid: "me-uid" } as never, clients: {}, groups: {}, members: {}, channels: { 4: channel } });
    useUi.getState().openDialog({ kind: "channelEdit", mode: "edit", channel: 4 });
    await act(async () => root.render(<TooltipProvider><Dialogs /></TooltipProvider>));
    // Another admin removes Bob while the dialog is open.
    await act(async () => useSession.setState({ channels: { 4: { ...channel, access: { roles: [], users: ["me-uid"] } } } }));
    const button = [...document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button')].find(b => b.textContent === "Save")!;
    await act(async () => button.click());
    expect(mock.update).toHaveBeenCalledTimes(1);
    expect(mock.update.mock.calls[0]![0]).not.toHaveProperty("access");
  });
});
