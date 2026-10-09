import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Client } from "../proto/Client";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Event } from "../proto/Event";
import type { Welcome } from "../proto/Welcome";
import type { ConnectionStatus, Link } from "../net/connection";
import type { EventName, Op } from "../net/protocol";
import { welcome } from "../net/testing";
import type { VoiceEngine } from "../voice/engine";

// The controller is the browser glue; give it just enough of a browser.
type Ctl = typeof import("./controller").controller;
type Stores = typeof import("./stores");
let controller: Ctl;
let stores: Stores;

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
  stores.useUi.setState({ dialog: { kind: "none" }, joining: null });
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
