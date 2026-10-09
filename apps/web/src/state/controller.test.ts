import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Client } from "../proto/Client";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Event } from "../proto/Event";
import type { Welcome } from "../proto/Welcome";
import type { ConnectionStatus, Link } from "../net/connection";
import type { EventName, Op } from "../net/protocol";
import { welcome } from "../net/testing";
import type { VoiceEngine } from "../voice/engine";

const notifyMock = vi.hoisted(() => vi.fn());
vi.mock("../platform/notify", () => ({ notify: notifyMock }));

// The controller is the browser glue; give it just enough of a browser.
type Ctl = typeof import("./controller").controller;
type Stores = typeof import("./stores");
let controller: Ctl;
let stores: Stores;
let settings: typeof import("./settings");

class FakeLink implements Link {
  status: ConnectionStatus = { state: "online" };
  isOnline = true;
  requests: Array<{ op: string; d: unknown }> = [];
  private welcomeHandlers: Array<(w: Welcome, resync: boolean) => void> = [];
  private eventHandlers: Array<(e: Event) => void> = [];
  private statusHandlers: Array<(s: ConnectionStatus) => void> = [];
  failing = new Set<string>();

  onStatus(h: (s: ConnectionStatus) => void) {
    this.statusHandlers.push(h);
    return () => {};
  }
  onWelcome(h: (w: Welcome, resync: boolean) => void) {
    this.welcomeHandlers.push(h);
    return () => {};
  }
  on<E extends EventName>(_ev: E, _h: never) {
    return () => {};
  }
  onEvent(h: (e: Event) => void) {
    this.eventHandlers.push(h);
    return () => {};
  }
  async connect(): Promise<Welcome> {
    return welcome();
  }
  async request(op: Op, d: unknown): Promise<never> {
    this.requests.push({ op, d });
    if (this.failing.has(op)) throw new Error("boom");
    return {} as never;
  }
  retryNow() {}
  close() {}

  sent(op: string) {
    return this.requests.filter((r) => r.op === op);
  }
  welcomeNow(w: Welcome, resync = false) {
    for (const h of this.welcomeHandlers) h(w, resync);
  }
  emit(event: Event) {
    for (const h of this.eventHandlers) h(event);
  }
  status_(status: ConnectionStatus) {
    for (const h of this.statusHandlers) h(status);
  }
}

function fakeEngine() {
  const engine = {
    capabilities: { outputDeviceSelection: false },
    start: vi.fn(async () => {}),
    stop: vi.fn(async () => {}),
    retryMic: vi.fn(async () => {}),
    setMuted: vi.fn(),
    setDeafened: vi.fn(),
    setInputMode: vi.fn(),
    setPttActive: vi.fn(),
    setInputDevice: vi.fn(async () => {}),
    setOutputDevice: vi.fn(async () => {}),
    setCaptureOptions: vi.fn(async () => {}),
    setMasterVolume: vi.fn(),
    setUserVolume: vi.fn(),
    setSlotOwner: vi.fn(),
    listDevices: vi.fn(async () => ({ inputs: [], outputs: [] })),
    startMicTest: vi.fn(async () => () => {}),
    on: vi.fn(() => () => {}),
  };
  return engine satisfies VoiceEngine;
}

const client = (id: number, channel: number | null, extra: Partial<Client> = {}): Client => ({
  id,
  uid: `u${id}`,
  nickname: `n${id}`,
  channel,
  groups: [],
  platform: "web",
  muted: false,
  deafened: false,
  away: null,
  talking: false,
  voice: false,
  ...extra,
});

const chan = (id: number, extra: object = {}) => ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, ...extra });

const message = (id: number, channel: number, extra: Partial<ChatMessage> = {}): ChatMessage => ({
  id,
  target: { channel },
  author: 2,
  author_uid: "u2",
  author_name: "n2",
  text: `t${id}`,
  sent_at: 1000 + id,
  ...extra,
});

let engine: ReturnType<typeof fakeEngine>;
let link: FakeLink;

const session = () => stores.useSession.getState();

/** Connects a fake link; vc servers start users outside voice. */
function connectFake(opts: { channel?: number | null; kind?: "vc" | "teamspeak"; resync?: boolean } = {}) {
  const c = controller as unknown as { conn: Link | null; wire(l: Link): void };
  c.conn = link;
  c.wire(link);
  session().setAddress("test", opts.kind ?? "vc");
  link.welcomeNow(
    welcome({
      session: 1,
      uid: "u1",
      channels: [chan(1), chan(2), chan(3, { has_password: true })],
      clients: [client(1, opts.channel ?? null), client(2, 2)],
    }),
    opts.resync ?? false,
  );
}

beforeAll(async () => {
  const listeners = () => ({ addEventListener: vi.fn(), removeEventListener: vi.fn() });
  const storage = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => storage.get(k) ?? null,
    setItem: (k: string, v: string) => void storage.set(k, v),
    removeItem: (k: string) => void storage.delete(k),
  });
  vi.stubGlobal("window", { ...listeners(), location: { protocol: "http:", hostname: "localhost", port: "5173" } });
  vi.stubGlobal("document", { ...listeners(), visibilityState: "visible", hasFocus: () => true });
  stores = await import("./stores");
  settings = await import("./settings");
  // No real storage in this environment: keep the settings in memory only.
  settings.useSettings.persist.setOptions({ storage: { getItem: () => null, setItem: () => {}, removeItem: () => {} } });
  controller = (await import("./controller")).controller;
  engine = fakeEngine();
  controller.useEngine(engine);
  controller.init();
});

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  link = new FakeLink();
  session().dispatch({ type: "reset" });
  stores.useUi.setState({ dialog: { kind: "none" }, joining: null, toasts: [] });
  stores.useVoice.setState({ muted: false, deafened: false, micError: null, state: "idle" });
});

afterEach(async () => {
  vi.useRealTimers();
  await controller.disconnect(true);
});

describe("staying on the server without voice", () => {
  it("does not start voice (or ask for the microphone) after connecting", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).not.toHaveBeenCalled();
    expect(link.sent("voice.offer")).toHaveLength(0);
  });

  it("starts voice when I join and sends a fresh offer", async () => {
    connectFake();
    await controller.joinChannel(2);
    expect(link.sent("channel.join")[0]?.d).toEqual({ channel: 2, password: null });
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).toHaveBeenCalledTimes(1);
    const params = (engine.start.mock.calls[0] as unknown as [{ offer(sdp: string): Promise<string> }])[0];
    await params.offer("v=0");
    expect(link.sent("voice.offer")[0]?.d).toEqual({ sdp: "v=0" });
  });

  it("leaving voice sends channel.leave, stops the engine and stays online", async () => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).toHaveBeenCalledTimes(1);
    await controller.leaveVoice();
    expect(link.sent("channel.leave")).toHaveLength(1);
    expect(engine.stop).toHaveBeenCalled();
    link.emit({ ev: "client.updated", d: client(1, null) });
    expect(session().phase).toBe("online");
    expect(session().clients[1]).toBeDefined();
    // Joining again negotiates again.
    link.emit({ ev: "client.updated", d: client(1, 3) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).toHaveBeenCalledTimes(2);
  });

  it("a server-side removal from voice also releases the microphone", async () => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    engine.stop.mockClear();
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.stop).toHaveBeenCalledTimes(1);
  });

  it("does not retry voice while I am not in a channel", async () => {
    connectFake();
    link.emit({ ev: "voice.closed", d: { reason: "x" } });
    await vi.advanceTimersByTimeAsync(30_000);
    expect(engine.start).not.toHaveBeenCalled();
  });

  it("goes back into the voice channel after a reconnect", async () => {
    connectFake({ channel: 2 });
    await vi.advanceTimersByTimeAsync(10);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    expect(engine.stop).toHaveBeenCalled();
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 2, password: null });
  });

  it("does not offer leaving on TeamSpeak servers", async () => {
    connectFake({ channel: 1, kind: "teamspeak" });
    await controller.leaveVoice();
    expect(link.sent("channel.leave")).toHaveLength(0);
    expect(controller.canLeaveVoice).toBe(false);
  });
});

describe("opening channel chats", () => {
  it("selecting a channel shows its chat without joining and loads its history once", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    const before = link.sent("chat.history").length;
    controller.selectChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    expect(session().viewChannel).toBe(2);
    expect(link.sent("channel.join")).toHaveLength(0);
    expect(link.sent("chat.history").at(-1)?.d).toMatchObject({ channel: 2 });
    expect(link.sent("chat.history").length).toBe(before + 1);
    controller.selectChannel(1);
    controller.selectChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("chat.history").length).toBe(before + 1);
    expect(controller.resolveTarget("channel")).toEqual({ channel: 2 });
  });

  it("offers the password dialog for locked channels instead of loading them", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    controller.selectChannel(3);
    await vi.advanceTimersByTimeAsync(10);
    expect(stores.useUi.getState().dialog).toEqual({ kind: "channelPassword", channel: 3 });
    expect(session().threads["ch:3"]?.locked).toBe(true);
    expect(link.sent("chat.history").some((r) => (r.d as { channel: number }).channel === 3)).toBe(false);
    // Entering with the password unlocks the chat and loads it.
    await controller.joinChannel(3, "pw");
    link.emit({ ev: "client.updated", d: client(1, 3) });
    await vi.advanceTimersByTimeAsync(10);
    expect(session().threads["ch:3"]?.locked).toBe(false);
    expect(link.sent("chat.history").some((r) => (r.d as { channel: number }).channel === 3)).toBe(true);
    // ...and it stays readable after leaving voice.
    controller.selectChannel(1);
    controller.selectChannel(3);
    expect(stores.useUi.getState().dialog.kind).toBe("channelPassword");
  });

  it("double click = open the chat and join voice", async () => {
    connectFake();
    await controller.joinChannelInteractive(2);
    expect(session().viewChannel).toBe(2);
    expect(link.sent("channel.join")[0]?.d).toEqual({ channel: 2, password: null });
  });
});

describe("telling the server what I have read", () => {
  it("reports the newest message of the open chat once, after a pause", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    for (const id of [10, 11, 12]) link.emit({ ev: "chat.message", d: message(id, 1) });
    expect(link.sent("chat.read")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(999);
    expect(link.sent("chat.read")).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(2);
    expect(link.sent("chat.read")).toEqual([{ op: "chat.read", d: { channel: 1, message: 12 } }]);
    // Nothing new: nothing more is sent.
    await vi.advanceTimersByTimeAsync(5000);
    expect(link.sent("chat.read")).toHaveLength(1);
    // The server's echo does not trigger another report either.
    link.emit({ ev: "chat.read", d: { channel: 1, message: 12 } });
    await vi.advanceTimersByTimeAsync(5000);
    expect(link.sent("chat.read")).toHaveLength(1);
    // A later message goes out with the next tick.
    link.emit({ ev: "chat.message", d: message(13, 1) });
    await vi.advanceTimersByTimeAsync(1001);
    expect(link.sent("chat.read")).toHaveLength(2);
    expect(link.sent("chat.read")[1]?.d).toEqual({ channel: 1, message: 13 });
  });

  it("does not report messages in channels I am not looking at, and reports them once I open the channel", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    link.emit({ ev: "chat.message", d: message(20, 2) });
    await vi.advanceTimersByTimeAsync(3000);
    expect(link.sent("chat.read")).toHaveLength(0);
    expect(session().threads["ch:2"]?.unread).toBe(1);
    controller.selectChannel(2);
    expect(session().threads["ch:2"]?.unread).toBe(0);
    await vi.advanceTimersByTimeAsync(1001);
    expect(link.sent("chat.read").at(-1)?.d).toEqual({ channel: 2, message: 20 });
  });

  it("does not report while the window is not focused", async () => {
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(30, 1) });
    await vi.advanceTimersByTimeAsync(3000);
    expect(link.sent("chat.read")).toHaveLength(0);
    expect(session().threads["ch:1"]?.unread).toBe(1);
    session().dispatch({ type: "focus", focused: true });
    await vi.advanceTimersByTimeAsync(1001);
    expect(link.sent("chat.read")).toHaveLength(1);
    expect(session().threads["ch:1"]?.unread).toBe(0);
  });

  it("keeps unread purely local on TeamSpeak servers", async () => {
    connectFake({ channel: 1, kind: "teamspeak" });
    await vi.advanceTimersByTimeAsync(10);
    link.emit({ ev: "chat.message", d: message(40, 2) });
    link.emit({ ev: "chat.message", d: message(41, 1) });
    await vi.advanceTimersByTimeAsync(5000);
    expect(link.sent("chat.read")).toHaveLength(0);
    expect(session().threads["ch:2"]?.unread).toBe(1);
  });
});

describe("sending, editing and deleting", () => {
  it("sends mentions and attachments along with the text", async () => {
    connectFake();
    await controller.sendChat("channel", "hi @n2", { mentions: ["u2"], attachments: ["f1"] });
    expect(link.sent("chat.send").at(-1)?.d).toEqual({ target: { channel: 1 }, text: "hi @n2", mentions: ["u2"], attachments: ["f1"] });
    await controller.sendChat("channel", "plain");
    expect(link.sent("chat.send").at(-1)?.d).toEqual({ target: { channel: 1 }, text: "plain" });
  });

  it("keeps mentions and attachments of a failed send for the retry", async () => {
    connectFake();
    link.failing.add("chat.send");
    await controller.sendChat("channel", "hi", { mentions: ["u2"], attachments: ["f1"] });
    expect(stores.useOutbox.getState().failed[0]).toMatchObject({ text: "hi", mentions: ["u2"], attachments: ["f1"] });
    link.failing.clear();
    await controller.retrySend(stores.useOutbox.getState().failed[0]!.id);
    expect(link.sent("chat.send").at(-1)?.d).toMatchObject({ mentions: ["u2"], attachments: ["f1"] });
  });

  it("edits and deletes through the server", async () => {
    connectFake();
    expect(await controller.editMessage(5, "new", ["u2"])).toBe(true);
    expect(link.sent("chat.edit")).toEqual([{ op: "chat.edit", d: { message: 5, text: "new", mentions: ["u2"] } }]);
    expect(await controller.deleteMessage(5)).toBe(true);
    expect(link.sent("chat.delete")).toEqual([{ op: "chat.delete", d: { message: 5 } }]);
    link.failing.add("chat.delete");
    expect(await controller.deleteMessage(6)).toBe(false);
    expect(stores.useUi.getState().toasts).toHaveLength(1);
  });

  it("applies edit and delete events to the thread", async () => {
    connectFake();
    link.emit({ ev: "chat.message", d: message(10, 1) });
    link.emit({ ev: "chat.edited", d: message(10, 1, { text: "changed", edited_at: 99 }) });
    expect(session().threads["ch:1"]?.items.some((i) => i.kind === "msg" && i.msg.text === "changed")).toBe(true);
    link.emit({ ev: "chat.deleted", d: { channel: 1, message: 10 } });
    expect(session().threads["ch:1"]?.items.some((i) => i.key === "m10")).toBe(false);
  });
});

describe("moderation requests", () => {
  it("sends role, ban and invite requests as the protocol defines them", async () => {
    connectFake();
    await controller.createGroup({ name: "Mods", permissions: ["client_kick"], color: "#ff0000" });
    await controller.updateGroup({ group: 3, color: "" });
    await controller.setMemberGroups("u2", [2, 3]);
    await controller.createBan({ client: 2, ip: true, duration: 3600, reason: "spam" });
    await controller.createInvite({ max_uses: 5, expires_in: 1800, group: 3 });
    await controller.deleteInvite("code");
    await controller.deleteBan(4);
    await controller.deleteGroup(3);
    expect(link.requests.map((r) => r.op).filter((op) => op !== "chat.history")).toEqual([
      "group.create",
      "group.update",
      "member.groups",
      "ban.create",
      "invite.create",
      "invite.delete",
      "ban.delete",
      "group.delete",
    ]);
    expect(link.sent("ban.create")[0]?.d).toEqual({ client: 2, ip: true, duration: 3600, reason: "spam" });
  });
});

describe("notifications", () => {
  const prefs = (patch: object = {}) =>
    settings.useSettings.getState().setNotifications({ mentions: true, privateMessages: true, allMessages: false, ...patch });

  beforeEach(() => {
    notifyMock.mockClear();
    prefs();
  });

  it("notifies about a mention while the window is in the background, and clicking opens that chat", async () => {
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(50, 2, { mentions: ["u1"], text: "hey you" }) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
    const call = notifyMock.mock.calls[0]![0] as { title: string; body: string; onClick: () => void };
    expect(call.title).toContain("n2");
    expect(call.body).toBe("hey you");
    call.onClick();
    expect(session().viewChannel).toBe(2);
    expect(session().activeThread).toBe("channel");
  });

  it("is quiet while the window is in front, for plain channel messages, and for my own", async () => {
    connectFake();
    session().dispatch({ type: "focus", focused: true });
    link.emit({ ev: "chat.message", d: message(51, 2, { mentions: ["u1"] }) });
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(52, 2) });
    link.emit({ ev: "chat.message", d: message(53, 2, { author: 1, author_uid: "u1", mentions: ["u1"] }) });
    expect(notifyMock).not.toHaveBeenCalled();
  });

  it("notifies about private messages and opens the conversation", async () => {
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(54, 1, { target: { client: 1 }, text: "psst" }) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
    (notifyMock.mock.calls[0]![0] as { onClick: () => void }).onClick();
    expect(session().activeThread).toBe("dm:u2");
  });

  it("can notify about everything", async () => {
    prefs({ allMessages: true });
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(55, 2) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
  });
});

describe("uploads", () => {
  it("refuses files over the server limit before asking the server", async () => {
    connectFake();
    stores.useSession.setState({ server: { ...session().server!, upload_limit: 100 } });
    await expect(controller.uploadFile({ name: "big.bin", size: 101, type: "" } as File)).rejects.toMatchObject({ kind: "too_large" });
    await expect(controller.uploadFile({ name: "e.bin", size: 0, type: "" } as File)).rejects.toMatchObject({ kind: "empty" });
    expect(link.sent("file.upload")).toHaveLength(0);
  });

  it("is off when the server takes no uploads, and on TeamSpeak", async () => {
    connectFake();
    expect(controller.canUpload).toBe(false);
    stores.useSession.setState({ server: { ...session().server!, upload_limit: 100 }, permissions: ["file_upload"] });
    expect(controller.canUpload).toBe(true);
    session().setAddress("ts", "teamspeak");
    expect(controller.canUpload).toBe(false);
  });
});
