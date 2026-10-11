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
    expect(link.sent("chat.send").at(-1)?.d).toMatchObject({ target: { channel: 1 }, text: "hi @n2", mentions: ["u2"], attachments: ["f1"], signature: expect.any(Object) });
    await controller.sendChat("channel", "plain");
    expect(link.sent("chat.send").at(-1)?.d).toMatchObject({ target: { channel: 1 }, text: "plain", signature: expect.any(Object) });
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

  type Verification = import("../net/message-signature").MessageVerification;
  const verifying = (result: Verification) =>
    vi.spyOn(controller as unknown as { verifyIncoming: (msg: ChatMessage) => Promise<Verification> }, "verifyIncoming").mockResolvedValue(result);
  const signed = {} as NonNullable<ChatMessage["signature"]>;

  it("edits and deletes my verified messages with a signature, others through the server", async () => {
    connectFake();
    const verify = verifying({ status: "verified", continuity: "proven", authorization: [] });
    try {
      link.emit({ ev: "chat.message", d: message(5, 1, { author: 1, author_uid: "u1", signature: signed }) });
      await vi.advanceTimersByTimeAsync(10);
      expect(await controller.editMessage(5, "new", ["u2"])).toBe(true);
      expect(link.sent("chat.edit")).toEqual([{ op: "chat.edit", d: { message: 5, text: "new", mentions: ["u2"], signature: expect.any(Object) } }]);
      expect(await controller.deleteMessage(5)).toBe(true);
      expect(link.sent("chat.edit").at(-1)?.d).toMatchObject({ message: 5, text: "\0gwar deleted v1" });
      expect(link.sent("chat.delete")).toEqual([]);
      link.emit({ ev: "chat.message", d: message(6, 1) });
      expect(await controller.deleteMessage(6)).toBe(true);
      expect(link.sent("chat.delete")).toEqual([{ op: "chat.delete", d: { message: 6 } }]);
      link.failing.add("chat.delete");
      expect(await controller.deleteMessage(7)).toBe(false);
      expect(stores.useUi.getState().toasts).toHaveLength(1);
    } finally { verify.mockRestore(); }
  });

  it("never signs an edit or deletion of a message that does not verify here", async () => {
    connectFake();
    const verify = verifying({ status: "verified", continuity: "proven", authorization: [] });
    // A genuine envelope with a grafted attachment: displayed (as invalid), never endorsed.
    const grafted = message(8, 1, { author: 1, author_uid: "u1", signature: signed,
      attachments: [{ id: "evil", name: "invoice.pdf", url: "/files/evil/invoice.pdf", size: 1, mime: "application/pdf", sha256: "x" }] });
    try {
      link.emit({ ev: "chat.message", d: grafted });
      await vi.advanceTimersByTimeAsync(10);
      for (const result of [
        { status: "invalid", continuity: "unproven", authorization: [] },
        { status: "verified", continuity: "fork", authorization: [] },
        { status: "verified", continuity: "proven", authorization: ["device-revoked"] },
      ] as Verification[]) {
        verify.mockResolvedValue(result);
        expect(await controller.editMessage(8, "only the text changed", [])).toBe(false);
        expect(await controller.deleteMessage(8)).toBe(true); // falls back to a server removal request
      }
      expect(link.sent("chat.edit")).toEqual([]);
      expect(stores.useUi.getState().toasts.at(0)?.text).toMatch(/could not be verified/);
    } finally { verify.mockRestore(); }
  });

  it("rejects unsigned edits and labels server removals", async () => {
    connectFake();
    link.emit({ ev: "chat.message", d: message(10, 1) });
    link.emit({ ev: "chat.edited", d: message(10, 1, { text: "changed", edited_at: 99 }) });
    expect(session().threads["ch:1"]?.items.some((i) => i.kind === "msg" && i.msg.text === "changed")).toBe(false);
    link.emit({ ev: "chat.deleted", d: { channel: 1, message: 10 } });
    expect(session().threads["ch:1"]?.items.some((i) => i.key === "m10" && i.kind === "msg" && i.removed)).toBe(true);
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
    link.emit({ ev: "chat.message", d: message(50, 2, { author_uid: "ts:peer", mentions: ["u1"], text: "hey you" }) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
    const call = notifyMock.mock.calls[0]![0] as { title: string; body: string; onClick: () => void };
    expect(call.title).toContain("n2");
    expect(call.title).toContain("TeamSpeak / unsigned");
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
    link.emit({ ev: "chat.message", d: message(54, 1, { author_uid: "ts:peer", target: { client: 1 }, text: "psst" }) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
    (notifyMock.mock.calls[0]![0] as { onClick: () => void }).onClick();
    expect(session().activeThread).toBe("dm:ts:peer");
  });

  it("can notify about everything", async () => {
    prefs({ allMessages: true });
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(55, 2, { author_uid: "ts:peer" }) });
    expect(notifyMock).toHaveBeenCalledTimes(1);
  });
});

it("suppresses unsigned Gwar mentions and DMs even when all notifications are enabled", () => {
  notifyMock.mockClear(); settings.useSettings.getState().setNotifications({ mentions: true, privateMessages: true, allMessages: true });
  connectFake(); session().dispatch({ type: "focus", focused: false });
  link.emit({ ev: "chat.message", d: message(60, 2, { mentions: ["u1"] }) });
  link.emit({ ev: "chat.message", d: message(61, 1, { target: { client: 1 } }) });
  expect(notifyMock).not.toHaveBeenCalled();
});

describe("uploads", () => {
  it("refuses server substitutions of locally derived upload names and MIME", async () => {
    connectFake(); stores.useSession.setState({ httpOrigin: "http://localhost:8799", server: { ...session().server!, upload_limit: 100 } });
    vi.spyOn(link, "request").mockResolvedValue({ file: "file", upload_url: "/api/files/file?token=x" } as never);
    const file = new File(["hello"], "notes.txt", { type: "text/plain" });
    const { sha256 } = await import("../net/message-signature");
    const att = { id: "file", name: "notes.txt", mime: "text/plain", size: 5, url: "/files/file/notes.txt", sha256: await sha256(new TextEncoder().encode("hello")) };
    const request = vi.spyOn(globalThis, "fetch");
    try {
      for (const changed of [{ ...att, name: "forged.txt" }, { ...att, mime: "text/html" }]) {
        request.mockResolvedValueOnce(new Response(JSON.stringify(changed)));
        await expect(controller.uploadFile(file)).rejects.toMatchObject({ kind: "network" });
      }
      request.mockResolvedValueOnce(new Response(JSON.stringify(att)));
      expect(await controller.uploadFile(file)).toBe("file");
    } finally { request.mockRestore(); }
  });
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

describe("bounded incoming verification", () => {
  it("verifies a signed file message once and notifies without fetching its bytes", async () => {
    connectFake(); session().dispatch({ type: "focus", focused: false });
    settings.useSettings.getState().setNotifications({ mentions: true, privateMessages: true, allMessages: false });
    const c = controller as unknown as { verifyIncoming: (msg: ChatMessage) => Promise<import("../net/message-signature").MessageVerification> };
    const verify = vi.spyOn(c, "verifyIncoming").mockResolvedValue({ status: "verified", continuity: "proven", authorization: [] });
    const fetch = vi.spyOn(globalThis, "fetch");
    try {
      const msg = message(62, 2, { mentions: ["u1"], signature: {} as NonNullable<ChatMessage["signature"]>,
        attachments: [{ id: "large", name: "large.bin", url: "/files/large/large.bin", size: 25 * 1024 * 1024, mime: "application/octet-stream", sha256: "signed-metadata" }] });
      link.emit({ ev: "chat.message", d: msg });
      await vi.advanceTimersByTimeAsync(10);
      expect(verify).toHaveBeenCalledExactlyOnceWith(msg);
      expect(fetch).not.toHaveBeenCalled(); expect(notifyMock).toHaveBeenCalledOnce();
      expect(session().threads["ch:2"]?.items.some((i) => i.kind === "msg" && i.msg.id === 62)).toBe(true);
    } finally { verify.mockRestore(); fetch.mockRestore(); }
  });
  it("discards a queued event when its epoch changes without verifying it again", async () => {
    connectFake();
    let resolve!: (v: import("../net/message-signature").MessageVerification) => void;
    const blocked = new Promise<import("../net/message-signature").MessageVerification>((r) => { resolve = r; });
    const c = controller as unknown as { verifyIncoming: (msg: ChatMessage) => Promise<import("../net/message-signature").MessageVerification> };
    const verify = vi.spyOn(c, "verifyIncoming").mockReturnValue(blocked);
    try {
      link.emit({ ev: "chat.message", d: message(63, 2, { signature: {} as NonNullable<ChatMessage["signature"]> }) });
      await vi.advanceTimersByTimeAsync(0);
      session().dispatch({ type: "reset" });
      resolve({ status: "verified", continuity: "proven", authorization: [] });
      await vi.advanceTimersByTimeAsync(10);
      expect(verify).toHaveBeenCalledOnce(); expect(session().threads).toEqual({});
    } finally { verify.mockRestore(); }
  });
  it("keeps the 256 queue bound and localizes the overflow toast", async () => {
    connectFake(); settings.useSettings.setState({ language: "pl" });
    let resolve!: (v: import("../net/message-signature").MessageVerification) => void;
    const blocked = new Promise<import("../net/message-signature").MessageVerification>((r) => { resolve = r; });
    const c = controller as unknown as { verifyIncoming: (msg: ChatMessage) => Promise<import("../net/message-signature").MessageVerification> };
    const verify = vi.spyOn(c, "verifyIncoming").mockResolvedValue({ status: "verified", continuity: "proven", authorization: [] }).mockReturnValueOnce(blocked);
    try {
      for (let id = 1000; id < 1257; id++) link.emit({ ev: "chat.message", d: message(id, 2, { signature: {} as NonNullable<ChatMessage["signature"]> }) });
      expect(stores.useUi.getState().toasts.at(-1)?.text).toBe("Kolejka weryfikacji wiadomości jest pełna");
      await vi.advanceTimersByTimeAsync(0);
      resolve({ status: "verified", continuity: "proven", authorization: [] });
      await vi.advanceTimersByTimeAsync(10);
      expect(verify).toHaveBeenCalledTimes(256);
    } finally { verify.mockRestore(); settings.useSettings.setState({ language: "en" }); }
  });
});

// Gate regressions (Astra r2 probes, inverted): production verifier, signing and reducer; scheduling is controlled.
import "fake-indexeddb/auto";
describe("edits stay on their connection; history ownership follows each verified row", () => {
  async function genuine(text = "genuine victim message") {
    const { keyFromSeed, signText } = await import("../connect/crypto");
    const { uidForPublicKey } = await import("../net/identity");
    const { signMessage, MessageVerifier } = await import("../net/message-signature");
    const { MessageStorage } = await import("../net/message-storage");
    const pair = await keyFromSeed(new Uint8Array(32).fill(37));
    const identity = { ...pair, nickname: "victim", sign: (bytes: Uint8Array) => signText(pair.jwk, new TextDecoder().decode(bytes)) } as unknown as import("../net/identity").Identity;
    const uid = await uidForPublicKey(pair.publicKey);
    const ctx = { serverKey: pair.publicKey, target: "channel:1" };
    const msg = message(71, 1, { author_uid: uid, text });
    msg.signature = await signMessage(identity, ctx, uid, text, [], []);
    const storage = new MessageStorage(`gate-${crypto.randomUUID()}`);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const c = controller as any;
    c.verifier = new MessageVerifier(storage);
    return { identity, uid, ctx, msg, storage, c };
  }
  async function servers(g: Awaited<ReturnType<typeof genuine>>) {
    const { Connection } = await import("../net/connection");
    const { keyFromSeed } = await import("../connect/crypto");
    const connection = (serverKey: string, identity = g.identity) => {
      const conn = Object.create(Connection.prototype);
      Object.assign(conn, { options: { identity }, serverKey_: serverKey, request: vi.fn(async () => ({})) });
      return conn;
    };
    const a = connection(g.ctx.serverKey), b = connection((await keyFromSeed(new Uint8Array(32).fill(38))).publicKey);
    g.c.conn = a;
    stores.useSession.setState({ me: { session: 1, uid: g.uid } });
    session().dispatch({ type: "event", now: Date.now(), event: { ev: "chat.message", d: g.msg }, authentic: true });
    const switchTo = () => {
      g.c.conn = b;
      session().dispatch({ type: "reset" });
      stores.useSession.setState({ me: { session: 1, uid: g.uid }, kind: "vc" });
    };
    return { a, b, connection, switchTo };
  }
  const gate = () => {
    let release!: () => void, entered!: () => void;
    const hold = new Promise<void>((r) => { release = r; }), started = new Promise<void>((r) => { entered = r; });
    return { release, started, pass: async () => { entered(); await hold; } };
  };

  it("never signs for or sends to another server when the connection changes during verification", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), { a, b, switchTo } = await servers(g), held = gate();
    const verify = g.c.verifier.verify.bind(g.c.verifier);
    g.c.verifier.verify = async (...args: unknown[]) => { const result = await verify(...args); await held.pass(); return result; };
    const sign = vi.spyOn(g.identity, "sign");
    const editing = controller.editMessage(g.msg.id, "private text intended for server A", []);
    await held.started; switchTo(); held.release();
    expect(await editing).toBe(false);
    expect(a.request).not.toHaveBeenCalled(); expect(b.request).not.toHaveBeenCalled(); expect(sign).not.toHaveBeenCalled();
    g.c.conn = link;
  });

  it("never sends a signature when the connection changes during signing", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), held = gate();
    const sign = g.identity.sign;
    g.identity.sign = async (bytes: Uint8Array) => { const signature = await sign(bytes); await held.pass(); return signature; };
    const { a, b, switchTo } = await servers(g);
    const deleting = controller.deleteMessage(g.msg.id);
    await held.started; switchTo(); held.release();
    expect(await deleting).toBe(false);
    expect(a.request).not.toHaveBeenCalled(); expect(b.request).not.toHaveBeenCalled();
    g.c.conn = link;
  });

  it("refuses a second change of the same message while the first is in flight", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), held = gate();
    const verify = g.c.verifier.verify.bind(g.c.verifier);
    g.c.verifier.verify = async (...args: unknown[]) => { const result = await verify(...args); await held.pass(); return result; };
    const { a } = await servers(g);
    const first = controller.editMessage(g.msg.id, "one", []);
    await held.started;
    expect(await controller.editMessage(g.msg.id, "two", [])).toBe(false);
    held.release();
    expect(await first).toBe(true);
    expect(a.request).toHaveBeenCalledTimes(1);
    expect(a.request.mock.calls[0][1]).toMatchObject({ message: g.msg.id, text: "one" });
    g.c.conn = link;
  });

  it("a new message is never sent, kept or retried elsewhere when the connection changes during signing (Astra r3 probe, inverted)", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), { a, b, switchTo } = await servers(g), held = gate();
    const sign = g.identity.sign;
    g.identity.sign = async (bytes: Uint8Array) => { const result = await sign(bytes); await held.pass(); return result; };
    stores.useSession.setState({ viewChannel: 1 });
    const sending = controller.sendChat("channel", "secret intended only for A");
    await held.started; switchTo();
    stores.useSession.setState({ viewChannel: 1, channels: { 1: chan(1) } });
    held.release();
    expect(await sending).toBe(false);
    expect(a.request).not.toHaveBeenCalled(); expect(b.request).not.toHaveBeenCalled();
    expect(stores.useOutbox.getState().failed).toEqual([]);
    g.c.conn = link;
  });

  it("a failed draft is retried only on the server it was written for", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), { b, switchTo } = await servers(g);
    switchTo();
    stores.useSession.setState({ viewChannel: 1, channels: { 1: chan(1) } });
    stores.useOutbox.getState().add({ thread: "ch:1", target: { channel: 1 }, text: "draft for A", serverKey: g.ctx.serverKey, error: "x" });
    const pending = stores.useOutbox.getState().failed[0]!;
    await controller.retrySend(pending.id);
    expect(b.request).not.toHaveBeenCalled();
    expect(stores.useOutbox.getState().failed).toEqual([]);
    g.c.conn = link;
  });

  it("an edit is not sent when a newer revision arrived while it was being signed (Astra r3 probe, inverted)", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), { a } = await servers(g), held = gate();
    const { signMessage } = await import("../net/message-signature");
    const newer = { ...g.msg, text: "earlier genuine edit from another device" };
    newer.signature = await signMessage(g.identity, g.ctx, g.uid, newer.text, [], [], g.msg.signature!);
    const sign = g.identity.sign;
    g.identity.sign = async (bytes: Uint8Array) => { const result = await sign(bytes); await held.pass(); return result; };
    const editing = controller.editMessage(g.msg.id, "competing edit", []);
    await held.started;
    session().dispatch({ type: "event", now: Date.now(), event: { ev: "chat.edited", d: newer }, authentic: true });
    held.release();
    expect(await editing).toBe(false);
    expect(a.request).not.toHaveBeenCalled();
    g.c.conn = link;
  });

  it("a forged duplicate in a history page never inherits the genuine row's ownership", async () => {
    vi.useRealTimers(); connectFake();
    const { msg, ctx, c } = await genuine();
    Object.defineProperty(link, "serverKey", { value: ctx.serverKey });
    const forged = { ...structuredClone(msg), text: "FORGED" };
    for (const page of [[forged, msg], [msg, forged]]) {
      session().dispatch({ type: "reset" }); connectFake();
      Object.defineProperty(link, "serverKey", { value: ctx.serverKey });
      await c.applyHistory(page, 1, null);
      const rows = session().threads["ch:1"]!.items.filter((i) => i.kind === "msg");
      expect(rows).toEqual([expect.objectContaining({ authentic: true, msg: expect.objectContaining({ text: msg.text }) })]);
      link.emit({ ev: "chat.message", d: { ...msg, id: 72 } });
      await c.messageQueue;
      expect(session().threads["ch:1"]!.items.filter((i) => i.kind === "msg").map((i) => i.kind === "msg" && i.msg.text)).toEqual([msg.text]);
    }
  });
});

describe("channel id reuse", () => {
  it("a channel recreated under an id this client saw deleted never verifies fully (Astra r2 probe, inverted)", async () => {
    vi.useRealTimers();
    const store = new Map<string, string>();
    vi.stubGlobal("localStorage", { getItem: (k: string) => store.get(k) ?? null, setItem: (k: string, v: string) => void store.set(k, v) });
    try {
      connectFake();
      const { keyFromSeed, signText } = await import("../connect/crypto");
      const { uidForPublicKey } = await import("../net/identity");
      const { signMessage, MessageVerifier, fullyVerified } = await import("../net/message-signature");
      const { MessageStorage } = await import("../net/message-storage");
      const pair = await keyFromSeed(new Uint8Array(32).fill(39));
      const identity = { ...pair, sign: (bytes: Uint8Array) => signText(pair.jwk, new TextDecoder().decode(bytes)) } as unknown as import("../net/identity").Identity;
      const uid = await uidForPublicKey(pair.publicKey);
      const msg = message(71, 1, { author_uid: uid, text: "old channel" });
      msg.signature = await signMessage(identity, { serverKey: pair.publicKey, target: "channel:1" }, uid, msg.text, [], []);
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const c = controller as any;
      c.verifier = new MessageVerifier(new MessageStorage(`reuse-${crypto.randomUUID()}`));
      Object.defineProperty(link, "serverKey", { value: pair.publicKey });
      await c.applyHistory([msg], 1, null);
      expect(fullyVerified(await c.verifyIncoming(msg, 1))).toBe(true);
      link.emit({ ev: "channel.deleted", d: { channel: 1 } });
      link.emit({ ev: "channel.created", d: chan(1, { name: "replacement channel" }) });
      await c.applyHistory([msg], 1, null);
      const result = await c.verifyIncoming(msg, 1);
      expect(result).toMatchObject({ status: "verified", authorization: ["channel-reused"] });
      expect(fullyVerified(result)).toBe(false);
    } finally { vi.unstubAllGlobals(); }
  });
});
