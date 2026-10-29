import type { Attachment } from "../proto/Attachment";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Client } from "../proto/Client";
import type { ChatMessage } from "../proto/ChatMessage";
import type { Event } from "../proto/Event";
import type { Permission } from "../proto/Permission";
import type { Welcome } from "../proto/Welcome";
import type { ConnectionStatus, Link } from "../net/connection";
import type { EventName, Op } from "../net/protocol";
import { welcome } from "../net/testing";
import type { VoiceEngine } from "../voice/engine";

// Native file commands, recording where uploads would go (only the desktop upload test turns the app native).
const native = vi.hoisted(() => ({ origin: null as string | null, session: null as string | null, sent: [] as Array<{ origin: string | null; bytes: Uint8Array }>, attachment: {} }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async (op: string, data: { origin?: string | null; session?: string | null }, options?: { headers: Record<string, string> }) => {
  if (op === "files_set_server") { native.origin = data.origin ?? null; native.session = data.session ?? null; return; }
  if (op === "upload_put") {
    // Like the native command: refused, before anything is sent, unless the connection that reserved it is current.
    if (!native.session || native.session !== options!.headers["x-upload-session"]) throw new Error("the server changed");
    native.sent.push({ origin: native.origin, bytes: data as unknown as Uint8Array });
    return native.attachment;
  }
}) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
const notifyMock = vi.hoisted(() => vi.fn());
vi.mock("../platform/notify", () => ({ notify: notifyMock }));

// The controller is the browser glue; give it just enough of a browser.
type Ctl = typeof import("./controller").controller;
type Stores = typeof import("./stores");
let controller: Ctl;
let stores: Stores;
let settings: typeof import("./settings");
let engineState: (state: import("../voice/engine").VoiceState) => void;

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
    return (op === "voice.offer" ? {...d as object,sdp:"answer"} : {}) as never;
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
    capabilities: { outputDeviceSelection: false, encryptedVoice: async () => true },
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
    on: vi.fn((_event: string, _handler: unknown) => () => {}),
  };
  return engine satisfies VoiceEngine;
}

const client = (id: number, channel: number | null, extra: Partial<Client> = {}): Client => ({
  id,
  uid: `u${id}`,
  nickname: `n${id}`,
  channel,
  voice_binding: channel === null ? null : { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1 },
  groups: [],
  platform: "web",
  muted: false,
  deafened: false,
  away: null,
  talking: false,
  voice: false,
  ...extra,
});

const chan = (id: number, extra: object = {}) => ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false, ...extra });

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

/** Connects a fake link; Gwar servers start users outside voice. */
function connectFake(opts: { channel?: number | null; kind?: "gwar" | "teamspeak"; resync?: boolean; encrypted?: number[]; serverChat?: boolean; permissions?: Permission[]; params?: import("./controller").ConnectParams } = {}) {
  const c = controller as unknown as { conn: Link | null; wire(l: Link): void };
  c.conn = link;
  (c.wire as (l: Link, p?: import("./controller").ConnectParams) => void)(link, opts.params);
  session().setAddress("test", opts.kind ?? "gwar");
  link.welcomeNow(
    welcome({
      session: 1,
      uid: "u1",
      server: { ...welcome().server, server_chat: { encrypted: opts.serverChat ?? false, history: opts.serverChat ?? false } },
      permissions: opts.permissions ?? [],
      channels: [chan(1), chan(2), chan(3, { has_password: true })].map(c => opts.encrypted?.includes(c.id) ? { ...c, encrypted: true } : c),
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
  engineState = engine.on.mock.calls.find(call => call[0] === "state")![1] as typeof engineState;
});

beforeEach(() => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  engine.capabilities.encryptedVoice = async () => true;
  link = new FakeLink();
  session().dispatch({ type: "reset" });
  stores.useUi.setState({ dialog: { kind: "none" }, joining: null, toasts: [] });
  stores.useVoice.setState({ muted: false, deafened: false, micError: null, state: "idle", reconnecting: false });
  settings.useSettings.setState({ selfMuted: false, selfDeafened: false, resumeConnection: null, updateVoiceResume: null, language: "en" });
  stores.useConnectUi.setState({ restoring: false, busy: false, error: null, invite: null });
  (controller as unknown as { restored: boolean; persistRequested: boolean }).restored = false;
  (controller as unknown as { persistRequested: boolean }).persistRequested = false;
  (controller as unknown as { voiceAutoStart: boolean }).voiceAutoStart = true;
  engine.start.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  vi.useRealTimers();
  await controller.disconnect(true);
});

describe("session resilience", () => {
  const saved = { address: "test", kind: "gwar" as const, name: "Friends", password: "secret", insecureLan: true };

  it("persists self mute and deafen, and applies both before starting the microphone", async () => {
    controller.setMuted(true);
    controller.setDeafened(true);
    expect(settings.useSettings.getState()).toMatchObject({ selfMuted: true, selfDeafened: true });
    engine.start.mockImplementationOnce(async () => {
      expect(engine.setMuted).toHaveBeenLastCalledWith(true);
      expect(engine.setDeafened).toHaveBeenLastCalledWith(true);
    });
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledOnce();
    controller.setMuted(false);
    expect(settings.useSettings.getState()).toMatchObject({ selfMuted: false, selfDeafened: false });
  });

  it("requests persistent storage once on welcome; refusal does not break the session or reconnect", async () => {
    const persist = vi.fn(async () => { throw new Error("denied"); });
    const nav = globalThis.navigator;
    vi.stubGlobal("navigator", { storage: { persist } });
    const c = controller as unknown as { conn: Link; wire(l: Link, p: typeof saved): void };
    c.conn = link; c.wire(link, saved);
    const w = welcome({ session: 1, uid: "u1", channels: [chan(1)], clients: [client(1, null)] });
    link.welcomeNow(w);
    link.welcomeNow(w, true);
    await vi.advanceTimersByTimeAsync(0);
    expect(persist).toHaveBeenCalledOnce();
    expect(settings.useSettings.getState().resumeConnection).toMatchObject({ ...saved, name: "Test" });
    expect(session().phase).toBe("online");
    vi.stubGlobal("navigator", nav);
  });

  it("restores once after startup without joining voice", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => {
      expect(params).not.toHaveProperty("channel");
      connectFake(); return true;
    });
    await controller.restoreSession(); await controller.restoreSession();
    expect(connect).toHaveBeenCalledExactlyOnceWith(saved, { remember: false });
    expect(engine.start).not.toHaveBeenCalled();
    expect(stores.useConnectUi.getState().restoring).toBe(false);
    connect.mockRestore();
  });

  it("a cold TeamSpeak restoration does not open capture in its default channel", async () => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => { connectFake({ kind: "teamspeak", channel: 1, params }); return true; });
    await controller.restoreSession();
    expect(engine.start).not.toHaveBeenCalled();
    connect.mockRestore();
  });

  it("an explicit disconnect clears startup and update intent", async () => {
    settings.useSettings.setState({ resumeConnection: saved, updateVoiceResume: { address: "test", kind: "gwar", version: __APP_VERSION__, channel: 1, expiresAt: Date.now() + 1000 } });
    await controller.disconnect();
    const connect = vi.spyOn(controller, "connectInteractive");
    await controller.restoreSession();
    expect(connect).not.toHaveBeenCalled();
    expect(settings.useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
    connect.mockRestore();
  });

  it("records update rejoin only while in voice and can cancel it after an installation failure", () => {
    connectFake({ channel: 2 });
    const cancel = controller.prepareUpdateRestart("0.2.0");
    expect(settings.useSettings.getState().updateVoiceResume).toEqual({ address: "test", kind: "gwar", version: "0.2.0", channel: 2, password: undefined, expiresAt: Date.now() + 300_000 });
    cancel();
    expect(settings.useSettings.getState().updateVoiceResume).toBeNull();
    connectFake(); controller.prepareUpdateRestart("0.2.0");
    expect(settings.useSettings.getState().updateVoiceResume).toBeNull();
  });

  it("cancelling while the identity loads prevents a late connection", async () => {
    let resolve!: (identity: import("../net/identity").Identity) => void;
    const identity = vi.spyOn(controller, "getIdentity").mockImplementation(() => new Promise(yes => { resolve = yes; }));
    const connecting = controller.connect({ address: "ws://localhost:18979/ws" });
    // Let disconnect/teardown finish so the identity request is in progress.
    await vi.advanceTimersByTimeAsync(0);
    await controller.disconnect();
    resolve({ nickname: "me" } as import("../net/identity").Identity);
    await expect(connecting).rejects.toThrow();
    expect(session().phase).toBe("idle");
    expect(settings.useSettings.getState().resumeConnection).toBeNull();
    identity.mockRestore();
  });

  it("an invite takes precedence over restoring the previous server", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    stores.useConnectUi.setState({ invite: { address: "other", code: "invite" } as never });
    const connect = vi.spyOn(controller, "connectInteractive");
    await controller.restoreSession();
    expect(connect).not.toHaveBeenCalled(); connect.mockRestore();
  });

  it("returns to the form after one failed restoration, without scheduled retries", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => {
      // The real connect invalidates earlier attempts via disconnect before it starts.
      await controller.disconnect(true);
      throw new Error("Cannot reach Friends");
    });
    await controller.restoreSession();
    await vi.advanceTimersByTimeAsync(120_000);
    expect(connect).toHaveBeenCalledOnce();
    expect(stores.useConnectUi.getState()).toMatchObject({ restoring: false, busy: false, error: "Cannot reach Friends" });
    expect(session().phase).toBe("idle");
    connect.mockRestore();
  });

  it.each(["valid", "expired", "other version", "other server"])("consumes an update voice intent once: %s", async variant => {
    settings.useSettings.setState({ resumeConnection: saved, updateVoiceResume: {
      address: variant === "other server" ? "other.example" : "test", kind: "gwar", channel: 2, password: "channel-secret", version: variant === "other version" ? "old" : __APP_VERSION__, expiresAt: Date.now() + (variant === "expired" ? -1 : 1000),
    } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async () => { connectFake(); return true; });
    await controller.restoreSession();
    expect(link.sent("channel.join")).toHaveLength(variant === "valid" ? 1 : 0);
    if (variant === "valid") expect(link.sent("channel.join")[0]!.d).toEqual({ channel: 2, password: "channel-secret" });
    expect(settings.useSettings.getState().updateVoiceResume).toBeNull();
    connect.mockRestore();
  });

  it("keeps retrying beyond five failures with at most 15 seconds between attempts, with one toast", async () => {
    const toast = vi.spyOn(stores.useUi.getState(), "toast");
    engine.start.mockRejectedValue(new Error("offline"));
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(0);
    for (let i = 0; i < 12; i++) {
      const before = engine.start.mock.calls.length;
      await vi.advanceTimersByTimeAsync(15_000);
      expect(engine.start.mock.calls.length).toBeGreaterThan(before);
    }
    expect(stores.useVoice.getState().reconnecting).toBe(true);
    expect(toast.mock.calls.filter(call => call[0] === "error")).toHaveLength(1);
    engineState("connected");
    expect(stores.useVoice.getState().reconnecting).toBe(false);
    const starts = engine.start.mock.calls.length;
    await vi.advanceTimersByTimeAsync(60_000);
    expect(engine.start).toHaveBeenCalledTimes(starts);
    engineState("failed");
    await vi.advanceTimersByTimeAsync(3000);
    expect(toast.mock.calls.filter(call => call[0] === "error")).toHaveLength(2);
    toast.mockRestore();
  });

  it.each(["leave", "disconnect", "server lost"])("cancels voice retries when %s", async action => {
    engine.start.mockRejectedValue(new Error("offline"));
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    if (action === "leave") link.emit({ ev: "client.updated", d: client(1, null) });
    if (action === "disconnect") await controller.disconnect();
    if (action === "server lost") { link.isOnline = false; link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 }); }
    await vi.advanceTimersByTimeAsync(60_000);
    expect(engine.start).toHaveBeenCalledOnce();
  });

  it("manual voice Retry replaces the pending retry instead of starting two", async () => {
    engine.start.mockRejectedValueOnce(new Error("offline"));
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    controller.retryVoice(); await vi.advanceTimersByTimeAsync(60_000);
    expect(engine.start).toHaveBeenCalledTimes(2);
  });

  it("asks before switching away from a voice call; cancel leaves the call intact", async () => {
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    const connect = vi.spyOn(controller, "connect").mockResolvedValue();
    await controller.connectInteractive({ address: "other.example" }, { remember: false });
    expect(connect).not.toHaveBeenCalled();
    const dialog = stores.useUi.getState().dialog;
    expect(dialog).toMatchObject({ kind: "confirm", body: expect.stringContaining("other.example") });
    stores.useUi.getState().closeDialog();
    expect(engine.stop).not.toHaveBeenCalled();
    await controller.connectInteractive({ address: "other.example" }, { remember: false });
    const confirmed = stores.useUi.getState().dialog;
    if (confirmed.kind === "confirm") confirmed.onConfirm();
    expect(connect).toHaveBeenCalledWith({ address: "other.example" });
    connect.mockRestore();
  });

  it("also guards screen sharing without voice", async () => {
    connectFake();
    const { useScreen } = await import("./screen");
    useScreen.setState({ own: { phase: "live" } as never });
    const connect = vi.spyOn(controller, "connect").mockResolvedValue();
    await controller.connectInteractive({ address: "other.example" }, { remember: false });
    expect(connect).not.toHaveBeenCalled();
    expect(stores.useUi.getState().dialog.kind).toBe("confirm");
    useScreen.setState({ own: null }); connect.mockRestore();
  });
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
    const params = (engine.start.mock.calls[0] as unknown as [{ offer(sdp: string,run:number): Promise<string> }])[0];
    // A native offer: the receive section announces its own SSRC; only the microphone's is sent.
    await params.offer("v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=sendonly\r\na=ssrc:123 cname:mic\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=recvonly\r\na=ssrc:456 cname:slot\r\n", 1);
    expect(link.sent("voice.offer")[0]?.d).toMatchObject({channel:2,ssrc:123,run:1,group:null,call:"AAAAAAAAAAAAAAAAAAAAAA",participation:"AQEBAQEBAQEBAQEBAQEBAQ"});
  });

  it.each(["move", "retry", "stop"])("does not toast an obsolete start failure after a %s", async (action) => {
    let reject!: (error: Error) => void;
    engine.start.mockImplementationOnce(() => new Promise<void>((_resolve, no) => { reject = no; }));
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    if (action === "move") link.emit({ ev: "client.updated", d: client(1, 2) });
    else if (action === "retry") controller.retryVoice();
    else await controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(10);
    const starts = action === "stop" ? 1 : 2;
    expect(engine.start).toHaveBeenCalledTimes(starts);
    reject(new Error("obsolete negotiation"));
    await vi.advanceTimersByTimeAsync(10);
    expect(stores.useUi.getState().toasts.filter(t => t.kind === "error")).toEqual([]);
    expect(engine.start).toHaveBeenCalledTimes(starts);
  });

  it("stops pending plaintext voice on a forced encrypted move without starting encrypted capture", async () => {
    let finish!: () => void;
    engine.start.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    connectFake({ channel: 1, encrypted: [2] });
    await vi.advanceTimersByTimeAsync(10);
    engine.stop.mockClear();
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.stop).toHaveBeenCalled();
    expect(engine.start).toHaveBeenCalledTimes(1);
    finish();
    await vi.advanceTimersByTimeAsync(10);
    // Without an MLS call controller there is no encrypted start and no plaintext fallback.
    expect(engine.start).toHaveBeenCalledTimes(1);
    expect(stores.useVoice.getState().state).not.toBe("connected");
  });

  it("does not clear replacement voice state when an older stop finishes", async () => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    let finish!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    link.emit({ ev: "client.updated", d: client(1, 2) });
    controller.retryVoice();
    stores.useVoice.getState().set({ state: "connected" });
    finish();
    await vi.advanceTimersByTimeAsync(10);
    expect(stores.useVoice.getState().state).toBe("connected");
    expect(engine.start).toHaveBeenCalledTimes(2);
  });

  it("refuses encrypted voice before joining or opening the microphone when the engine cannot", async () => {
    connectFake({ encrypted: [2] });
    engine.capabilities.encryptedVoice = async () => false;
    await controller.joinChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join")).toHaveLength(0);
    expect(engine.start).not.toHaveBeenCalled();
    expect(stores.useUi.getState().toasts.at(-1)?.text).toBe("Update your browser or use the desktop app to join encrypted voice.");
  });

  it("a newer voice action cancels an encrypted join still waiting for its probe", async () => {
    connectFake({ channel: 1, encrypted: [2] });
    await vi.advanceTimersByTimeAsync(10);
    let finish!: (ok: boolean) => void;
    engine.capabilities.encryptedVoice = () => new Promise<boolean>(resolve => { finish = resolve; });
    const joining = controller.joinChannel(2);
    await controller.leaveVoice();
    finish(true);
    await joining;
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join")).toHaveLength(0);
  });

  it("an older move completing does not cancel a newer encrypted join", async () => {
    connectFake({ channel: 1, encrypted: [3] });
    await vi.advanceTimersByTimeAsync(10);
    let finish!: (ok: boolean) => void;
    engine.capabilities.encryptedVoice = () => new Promise<boolean>(resolve => { finish = resolve; });
    const joining = controller.joinChannel(3);
    // The earlier move to plaintext channel 2 completes meanwhile.
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(10);
    finish(true);
    await joining;
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").at(-1)?.d).toMatchObject({ channel: 3 });
  });

  it("a refused forced move into an encrypted channel offers Retry", async () => {
    connectFake({ channel: 1, encrypted: [2] });
    await vi.advanceTimersByTimeAsync(10);
    engine.capabilities.encryptedVoice = async () => false;
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(10);
    expect(stores.useVoice.getState().state).toBe("failed");
  });

  it("joins an encrypted channel when the engine can encrypt", async () => {
    connectFake({ encrypted: [2] });
    await controller.joinChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join")).toHaveLength(1);
  });

  it("leaves an unconfirmed call replacement and ignores an old-run closure", async () => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    engine.stop.mockClear();
    link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 2, reason: "transport_failed" } });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.stop).not.toHaveBeenCalled();
    link.emit({ ev: "client.updated", d: client(1, 1, { voice_binding: { call: "AgICAgICAgICAgICAgICAg", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1 } }) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.stop).toHaveBeenCalled();
    expect(link.sent("channel.leave")).toHaveLength(1);
    expect(engine.start).toHaveBeenCalledTimes(1);
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
    link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason: "transport_failed" } });
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

describe("a channel hidden from me", () => {
  it("closes its open image and loads its history again when it is shown again", async () => {
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    const att = { id: "f1", name: "a.png", mime: "image/png", size: 1, url: "/files/f1", sha256: "x" } as never;
    link.emit({ ev: "chat.message", d: message(40, 2, { attachments: [att] }) });
    controller.selectChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    const loads = () => link.sent("chat.history").filter((f) => (f.d as { channel: number }).channel === 2).length;
    const before = loads();
    expect(before).toBeGreaterThan(0);
    stores.useUi.getState().openDialog({ kind: "lightbox", attachment: att, verified: true });
    link.emit({ ev: "channel.hidden", d: { channel: 2 } });
    expect(stores.useUi.getState().dialog.kind).toBe("none");
    link.emit({ ev: "channel.created", d: chan(2) });
    controller.selectChannel(2);
    await vi.advanceTimersByTimeAsync(10);
    expect(loads()).toBe(before + 1);
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

describe("TeamSpeak actions", () => {
  const teamspeak = () => connectFake({ channel: 1, kind: "teamspeak", permissions: ["connect", "kick_members", "manage_channels"] });

  it("shows a poke and keeps it out of the session", async () => {
    teamspeak();
    const before = session();
    link.emit({ ev: "client.poked", d: { client: 99, nickname: "bob", message: "look" } });
    expect(stores.useUi.getState().toasts.map((t) => t.text)).toEqual(["bob pokes you: look"]);
    expect(session().threads).toEqual(before.threads);
  });

  it("shows a poke without formatting that would reorder or break its text", async () => {
    teamspeak();
    link.emit({ ev: "client.poked", d: { client: 99, nickname: "b\u202eob", message: "line\none\u2066 two" } });
    expect(stores.useUi.getState().toasts.map((t) => t.text)).toEqual(["bob pokes you: line one two"]);
  });

  it("ignores pokes anywhere but on TeamSpeak, where nothing verifies who they are from", async () => {
    connectFake({ channel: 1 });
    link.emit({ ev: "client.poked", d: { client: 99, nickname: "bob", message: "look" } });
    expect(stores.useUi.getState().toasts).toEqual([]);
  });

  it("pokes, kicks from the channel, reads descriptions and moves channels among their siblings", async () => {
    teamspeak();
    await controller.pokeClient(2, "u2", "hi");
    expect(link.sent("client.poke").at(-1)?.d).toEqual({ client: 2, uid: "u2", message: "hi" });
    await controller.moveClient(2, "u2", 3);
    expect(link.sent("client.move").at(-1)?.d).toEqual({ client: 2, uid: "u2", channel: 3 });
    await controller.kickFromChannel(2, "u2");
    expect(link.sent("client.kick").at(-1)?.d).toEqual({ client: 2, uid: "u2", reason: null, from_channel: true });
    for (const c of [{ id: 1, position: 0 }, { id: 2, position: 1 }, { id: 3, position: 2 }]) {
      link.emit({ ev: "channel.updated", d: { ...session().channels[c.id] ?? chan(c.id), position: c.position, parent: null } });
    }
    await controller.moveChannelBy(2, 1);
    expect(link.sent("channel.update").at(-1)?.d).toEqual({ channel: 2, position: 2 });
    await controller.moveChannelBy(2, -1);
    expect(link.sent("channel.update").at(-1)?.d).toEqual({ channel: 2, position: 0 });
    const updates = link.sent("channel.update").length;
    await controller.moveChannelBy(1, -1);
    await controller.moveChannelBy(3, 1);
    expect(link.sent("channel.update")).toHaveLength(updates);
  });
});

describe("where a connection opens", () => {
  type Quiet = { conn: Link | null; connectionParams: unknown };
  const ui = () => stores.useUi.getState();
  // Where the person was: a settings tab, opened over the members of a conversation.
  const inSettings = () => {
    stores.useUi.setState({ section: "servers", pane: "members", overview: false });
    ui().openSettings("account");
  };

  it("one the user asks for opens the server's channel list, the same server again after a drop too", async () => {
    connectFake();
    stores.useUi.setState({ section: "servers", pane: "members", overview: true });
    await controller.disconnect(true);
    expect(session().address).toBe("test");
    inSettings();
    const connect = vi.spyOn(controller, "connect").mockResolvedValue();
    await controller.connectInteractive({ address: "test" }, { remember: false });
    expect(connect).toHaveBeenCalled();
    expect(ui()).toMatchObject({ section: "servers", pane: "list", overview: false });
  });

  it("a quiet reconnect for a renewed certificate stays where the person is, and keeps what belongs to them", async () => {
    const { Connection } = await import("../net/connection");
    const identity = (cert: string) => ({ nickname: "me", publicKey: "k", device: { certificate: cert } }) as unknown as import("../net/identity").Identity;
    const c = controller as unknown as Quiet;
    c.conn = Object.create(Connection.prototype, { identity: { value: identity("old") }, close: { value: () => {} } }) as Link;
    c.connectionParams = { address: "test" };
    inSettings();
    const replaced = (await import("./identity")).useAccountIdentity.getState().replaced;
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => {
      session().dispatch({ type: "phase", phase: "connecting" });
      session().dispatch({ type: "phase", phase: "online" });
    });
    await controller.replaceIdentityQuietly(identity("renewed"));
    expect(connect).toHaveBeenCalledWith({ address: "test" });
    expect(ui()).toMatchObject({ section: "settings", settingsTab: "account", settingsDrill: true, pane: "members" });
    expect((await import("./identity")).useAccountIdentity.getState().replaced).toBe(replaced);
  });

  it("another identity counts as a replacement: signing in or out", async () => {
    const { useAccountIdentity } = await import("./identity");
    const before = useAccountIdentity.getState().replaced;
    await controller.switchIdentity({ nickname: "someone", publicKey: "k2" } as unknown as import("../net/identity").Identity);
    await controller.switchIdentity(null);
    expect(useAccountIdentity.getState().replaced).toBe(before + 2);
  });
});

describe("request errors", () => {
  it("say which part of a half-done change went through", async () => {
    const { RequestError } = await import("../net/connection");
    const { describeRequestError } = await import("./controller");
    const text = describeRequestError(new RequestError("partial", "the edit went through, the move did not (TeamSpeak: refused)"));
    expect(text).toBe("Only part of that went through (the edit went through, the move did not (TeamSpeak: refused))");
  });
});

describe("joining password channels", () => {
  /** Like a TeamSpeak session: every action offered, the server decides. */
  const teamspeak = (channel = 1) => connectFake({ channel, kind: "teamspeak", permissions: ["bypass_password", "connect"] });
  /** Answers the next `channel.join` with an error, recording it like any request. */
  const refuseJoin = async (code: "wrong_password" | "forbidden" | "channel_full") => {
    const { RequestError } = await import("../net/connection");
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementationOnce(async (op, d) => {
      await request(op, d);
      throw new RequestError(code, "TeamSpeak: refused");
    });
  };

  it("joins without asking when the server lets me in (a TeamSpeak admin)", async () => {
    teamspeak();
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: null });
    expect(stores.useUi.getState().dialog.kind).toBe("none");
  });

  it("asks for the password only when refused, then remembers it for the session", async () => {
    teamspeak();
    await refuseJoin("wrong_password");
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: null });
    expect(stores.useUi.getState().dialog).toEqual({ kind: "channelPassword", channel: 3 });
    expect(stores.useUi.getState().toasts).toEqual([]);
    // What the dialog does.
    await controller.joinChannel(3, "pw");
    link.emit({ ev: "client.updated", d: client(1, 3) });
    link.emit({ ev: "client.updated", d: client(1, 2) });
    stores.useUi.setState({ dialog: { kind: "none" } });
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: "pw" });
    expect(stores.useUi.getState().dialog.kind).toBe("none");
  });

  it("forgets a password that stopped working and asks again", async () => {
    teamspeak();
    await controller.joinChannel(3, "old");
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await refuseJoin("wrong_password");
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: "old" });
    expect(stores.useUi.getState().dialog).toEqual({ kind: "channelPassword", channel: 3 });
    stores.useUi.setState({ dialog: { kind: "none" } });
    await refuseJoin("wrong_password");
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: null });
  });

  it.each([
    ["forbidden", "You aren't allowed to join this channel."],
    ["channel_full", "That channel is full."],
  ] as const)("says why a %s channel stays closed", async (code, text) => {
    teamspeak();
    await refuseJoin(code);
    await controller.joinChannelInteractive(2);
    expect(stores.useUi.getState().toasts.at(-1)?.text).toBe(text);
    expect(stores.useUi.getState().dialog.kind).toBe("none");
  });

  it("goes back to my TeamSpeak channel, password included, after a reconnect", async () => {
    teamspeak();
    await controller.joinChannel(3, "pw");
    link.emit({ ev: "client.updated", d: client(1, 3) });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    // TeamSpeak puts a returning client in the default channel.
    link.welcomeNow(welcome({ session: 9, uid: "u1", permissions: ["bypass_password", "connect"], channels: [chan(1), chan(3, { has_password: true })], clients: [client(9, 1, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: "pw" });
  });

  it("forgets typed passwords on another server", async () => {
    teamspeak();
    await controller.joinChannel(3, "pw");
    link.emit({ ev: "client.updated", d: client(1, 2) });
    link = new FakeLink();
    teamspeak(2);
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: null });
  });

  /** Holds the next `channel.join` on the current link until the test answers it. */
  const holdJoin = () => {
    let answer!: (error?: Error) => void;
    vi.spyOn(link, "request").mockImplementationOnce((op, d) => {
      link.requests.push({ op, d });
      return new Promise<never>((resolve, reject) => { answer = (error) => (error ? reject(error) : resolve({} as never)); });
    });
    return (error?: Error) => answer(error);
  };

  it("keeps a late answer from an ended session to that session", async () => {
    teamspeak();
    const answer = holdJoin();
    const joined = controller.joinChannel(3, "pw-of-a");
    // Another server, whose channel 3 is something else.
    link = new FakeLink();
    teamspeak(2);
    stores.useUi.getState().setJoining(1);
    answer();
    expect(await joined).toBe(false);
    expect(stores.useUi.getState().joining).toBe(1);
    await controller.joinChannelInteractive(3);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 3, password: null });
  });

  it("asks for no password over another server when the refusal comes late", async () => {
    teamspeak();
    const answer = holdJoin();
    const joining = controller.joinChannelInteractive(3);
    link = new FakeLink();
    teamspeak(2);
    const { RequestError } = await import("../net/connection");
    answer(new RequestError("wrong_password", "TeamSpeak: channel invalid password"));
    await joining;
    expect(stores.useUi.getState().dialog.kind).toBe("none");
    expect(stores.useUi.getState().toasts).toEqual([]);
  });

  it("drops the spinner of a join a resync ended", async () => {
    teamspeak();
    const answer = holdJoin();
    const joined = controller.joinChannel(3);
    expect(stores.useUi.getState().joining).toBe(3);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    // The resync keeps me in channel 1, where I was.
    link.welcomeNow(welcome({ session: 9, uid: "u1", permissions: ["bypass_password", "connect"], channels: [chan(1), chan(3)], clients: [client(9, 1, { uid: "u1" })] }), true);
    expect(stores.useUi.getState().joining).toBeNull();
    answer();
    expect(await joined).toBe(false);
    expect(stores.useUi.getState().joining).toBeNull();
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
    await controller.createGroup({ name: "Mods", permissions: ["kick_members"], color: "#ff0000", hoist: false, mentionable: false });
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

  it("shows names and text without formatting that would reorder the notification", async () => {
    connectFake();
    session().dispatch({ type: "focus", focused: false });
    link.emit({ ev: "chat.message", d: message(56, 1, { author_uid: "ts:mal", author_name: "Mal\u202elory", target: { client: 1 }, text: "he\u202ey\u0007you\nthere" }) });
    const call = notifyMock.mock.calls[0]![0] as { title: string; body: string };
    expect(call.title).toContain("Mallory");
    expect(call.body).toBe("hey you\nthere");
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
  it("encrypts a sealed upload and keeps what it decrypts to for the message", async () => {
    connectFake(); stores.useSession.setState({ httpOrigin: "http://localhost:8799", server: { ...session().server!, upload_limit: 100 } });
    const request = vi.spyOn(link, "request").mockResolvedValue({ file: "file", upload_url: "/api/files/file?token=x" } as never);
    const { sha256 } = await import("../net/message-signature");
    const { openFile } = await import("../net/sealed-file");
    const plain = new TextEncoder().encode("hello");
    let sent!: Uint8Array<ArrayBuffer>;
    const reply = async (changed: object = {}) => ({ id: "file", name: "file", mime: "application/octet-stream", size: sent.length, url: "/files/file/file", sha256: await sha256(sent), ...changed });
    const put = vi.spyOn(globalThis, "fetch");
    try {
      // The server is told neither the name nor the type, and must not claim them back.
      for (const changed of [{ name: "notes.txt" }, { mime: "text/plain" }, { size: 5 }, {}]) {
        put.mockImplementationOnce(async (_url, init) => { sent = init!.body as Uint8Array<ArrayBuffer>; return new Response(JSON.stringify(await reply(changed))); });
        const upload = controller.uploadFile(new File([plain], "notes.txt", { type: "text/plain" }), { sealed: true });
        if (Object.keys(changed).length) await expect(upload).rejects.toMatchObject({ kind: "network" });
        else expect(await upload).toBe("file");
      }
      expect(request).toHaveBeenCalledWith("file.upload", { name: "file", size: 33, mime: "application/octet-stream", sha256: await sha256(sent), sealed: true });
      expect(sent).toHaveLength(5 + 28);
      const att = (controller as unknown as { uploaded: Map<string, { att: Attachment }> }).uploaded.get("file")!.att;
      expect(att).toMatchObject({ id: "file", name: "notes.txt", mime: "text/plain", size: 5, url: "/files/file/file", sha256: await sha256(plain), sealed: { sha256: await sha256(sent), size: 33 } });
      expect(new TextDecoder().decode(await openFile(sent, att.sealed!.key))).toBe("hello");
      // It is never attached to a plaintext message.
      request.mockClear();
      expect(await controller.sendChat("channel", "hi", { attachments: ["file"] })).toBe(false);
      expect(request).not.toHaveBeenCalledWith("chat.send", expect.anything());
    } finally { put.mockRestore(); }
  });
  it("refuses files over the server limit before asking the server", async () => {
    connectFake();
    stores.useSession.setState({ server: { ...session().server!, upload_limit: 100 } });
    await expect(controller.uploadFile({ name: "big.bin", size: 101, type: "" } as File)).rejects.toMatchObject({ kind: "too_large" });
    await expect(controller.uploadFile({ name: "e.bin", size: 0, type: "" } as File)).rejects.toMatchObject({ kind: "empty" });
    await expect(controller.uploadFile({ name: "e.bin", size: 0, type: "" } as File, { sealed: true })).rejects.toMatchObject({ kind: "empty" });
    await expect(controller.uploadFile({ name: "b.bin", size: 73, type: "" } as File, { sealed: true })).rejects.toMatchObject({ kind: "too_large" });
    expect(link.sent("file.upload")).toHaveLength(0);
  });

  it("tells a full storage quota apart from a network failure", async () => {
    connectFake(); stores.useSession.setState({ server: { ...session().server!, upload_limit: 100 } });
    const { RequestError } = await import("../net/connection");
    vi.spyOn(link, "request").mockRejectedValueOnce(new RequestError("quota_exceeded", "your upload quota is used up"));
    await expect(controller.uploadFile(new File(["hello"], "a.txt"))).rejects.toMatchObject({ kind: "quota" });
  });

  it("is off when the server takes no uploads, and on TeamSpeak", async () => {
    connectFake();
    expect(controller.canUpload).toBe(false);
    stores.useSession.setState({ server: { ...session().server!, upload_limit: 100 }, permissions: ["attach_files"] });
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
      stores.useSession.setState({ me: { session: 1, uid: g.uid }, kind: "gwar" });
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

  it("a desktop upload never reaches a server the app switched to meanwhile (Astra r4 probe, inverted)", async () => {
    vi.useRealTimers(); connectFake();
    const g = await genuine(), { a, switchTo } = await servers(g), held = gate();
    stores.useSession.setState({ httpOrigin: "https://a.example", fileSession: "sA", server: { ...session().server!, upload_limit: 100 } });
    const bytes = new TextEncoder().encode("private bytes for A");
    const file = new File([bytes], "secret.txt", { type: "text/plain" });
    a.request.mockResolvedValue({ file: "file", upload_url: "/api/files/file?token=x" });
    const { setConnectedServer } = await import("../platform/download");
    vi.stubGlobal("window", { __TAURI_INTERNALS__: {}, addEventListener: vi.fn(), removeEventListener: vi.fn() });
    native.sent.length = 0;
    await setConnectedServer("https://a.example", 100, "sA");
    // The session ends while the native side is still getting ready (here: during the reservation reply).
    a.request.mockImplementation(async () => { await held.pass(); return { file: "file", upload_url: "/api/files/file?token=x" }; });
    try {
      const pending = controller.uploadFile(file);
      const result = expect(pending).rejects.toMatchObject({ kind: "network" });
      await held.started; switchTo(); await setConnectedServer("https://b.example", 100, "sB"); held.release(); await result;
      expect(native.sent).toEqual([]);
      expect(g.c.uploaded.size).toBe(0);
      // Even if the interface did call the native command, it names the connection the upload was reserved on:
      // refused on another server, and after a reconnect to the same one.
      const { uploadFromApp } = await import("../platform/download");
      await expect(uploadFromApp("/api/files/file?token=x", bytes, 100, "sA")).rejects.toThrow("server changed");
      await setConnectedServer("https://a.example", 100, "sA2");
      await expect(uploadFromApp("/api/files/file?token=x", bytes, 100, "sA")).rejects.toThrow("server changed");
      expect(native.sent).toEqual([]);
    } finally { vi.unstubAllGlobals(); g.c.conn = link; }
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
  const cases = {
    stored: " (Astra r2 probe, inverted)",
    full: ", even when storage writes fail (Astra r3 probe, inverted)",
    churn: ", even when no write fits and 64 other servers pass through before writes recover (Astra r4 probe, inverted)",
  };
  for (const [mode, title] of Object.entries(cases)) it(`a channel recreated under an id this client saw deleted never verifies fully${title}`, async () => {
    vi.useRealTimers();
    const failing = mode !== "stored";
    const store = new Map<string, string>();
    let full = false;
    // The device's security state (lib/local-state.ts), as a map whose writes can be refused.
    const { localState } = await import("../lib/local-state");
    const get = vi.spyOn(localState, "get").mockImplementation((k) => store.get(k));
    // A map is always fresh: reading it again changes nothing.
    const checkedAt = vi.spyOn(localState, "checkedAt").mockImplementation(() => Date.now());
    const read = vi.spyOn(localState, "read").mockImplementation(async (k) => store.get(k));
    const update = vi.spyOn(localState, "update").mockImplementation(async (k, change) => {
      const next = change(store.get(k));
      if (next === undefined) return store.get(k);
      if (full && (mode === "churn" || !(next ?? "").includes("\"incomplete\":true"))) throw new DOMException("quota", "QuotaExceededError");
      if (next === null) store.delete(k); else store.set(k, next);
      return next ?? undefined;
    });
    try {
      connectFake();
      const { keyFromSeed, signText } = await import("../connect/crypto");
      const { uidForPublicKey } = await import("../net/identity");
      const { signMessage, MessageVerifier, fullyVerified } = await import("../net/message-signature");
      const { MessageStorage } = await import("../net/message-storage");
      const pair = await keyFromSeed(new Uint8Array(32).fill({ stored: 39, full: 40, churn: 41 }[mode]!));
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
      full = failing;
      link.emit({ ev: "channel.deleted", d: { channel: 1 } });
      link.emit({ ev: "channel.created", d: chan(1, { name: "replacement channel" }) });
      if (mode === "churn") {
        const { channels } = await import("../net/channel-lifecycle");
        for (let i = 0; i < 64; i++) channels.observe(`churn-${i}`, [1]);
        full = false;
        link.welcomeNow(welcome({ channels: [chan(1)], clients: [client(1, null)], uid }));
      }
      await c.applyHistory([msg], 1, null);
      const result = await c.verifyIncoming(msg, 1);
      // Under churn every other server's record is unsaved too: past the bound the whole session is uncertain.
      expect(result).toMatchObject({ status: "verified", authorization: [mode === "churn" ? "channel-history-incomplete" : "channel-reused"] });
      expect(fullyVerified(result)).toBe(false);
    } finally { vi.unstubAllGlobals(); get.mockRestore(); update.mockRestore(); checkedAt.mockRestore(); read.mockRestore(); }
  });
});

describe("native file sessions", () => {
  const welcomeAgain = (resync: boolean) => link.welcomeNow(welcome({ session: 1, uid: "u1", channels: [chan(1)], clients: [client(1, null)] }), resync);
  it("every welcome names a new connection for native file commands; TeamSpeak and leaving have none", async () => {
    connectFake();
    session().setAddress("test", "gwar", "https://a.example");
    welcomeAgain(false);
    const first = session().fileSession;
    expect(first).toMatch(/^[0-9a-f-]{36}$/);
    welcomeAgain(true); // a resync is a new session on the server too: older file work must not continue
    expect(session().fileSession).toMatch(/^[0-9a-f-]{36}$/);
    expect(session().fileSession).not.toBe(first);
    session().setAddress("ts", "teamspeak", null);
    welcomeAgain(false);
    expect(session().fileSession).toBeNull();
    session().setAddress("test", "gwar", "https://a.example");
    welcomeAgain(false);
    await controller.disconnect(true);
    expect(session().fileSession).toBeNull();
  });
});

describe("ending a session", () => {
  it("closes dialogs about the ended session but keeps a changed server identity, whatever the timing", async () => {
    const confirm = { kind: "confirm" as const, title: "t", body: "b", confirmLabel: "ok", onConfirm: () => {} };
    for (const [serverIdentity, kept] of [[true, true], [false, false]] as const) {
      connectFake();
      stores.useUi.getState().openDialog({ ...confirm, serverIdentity });
      await controller.disconnect(true);
      expect(stores.useUi.getState().dialog.kind).toBe(kept ? "confirm" : "none");
      stores.useUi.getState().closeDialog();
    }
  });
});


it("recounts a conversation's unread messages from the server after a removal", async () => {
  connectFake();
  const c = controller as unknown as { sealed: unknown };
  const dm = { remove: vi.fn(), refreshUnread: vi.fn(async () => {}) };
  c.sealed = dm;
  try {
    link.emit({ ev: "chat.sealed.removed", d: { conversation: "dm:x:y", ids: [1] } });
    await Promise.resolve();
    expect(dm.remove).toHaveBeenCalledWith("dm:x:y", [1]);
    expect(dm.refreshUnread).toHaveBeenCalledWith("dm:x:y", [1]);
  } finally { c.sealed = null; }
});

it("keeps a removal after a sealed message that arrived while MLS was starting", async () => {
  connectFake();
  const c = controller as unknown as { sealed: unknown; sealedReady: Promise<void> };
  let ready!: () => void;
  c.sealedReady = new Promise(resolve => { ready = resolve; });
  const order: string[] = [];
  c.sealed = { receive: vi.fn(async () => { order.push("receive"); }), remove: vi.fn(() => order.push("remove")), refreshUnread: vi.fn(async () => {}) };
  try {
    const row = { id: 1, conversation: "dm:x:y", group: "g", epoch: 0, author_uid: "x", author_name: "x", message_id: "m", revision: 0, ciphertext: "c", sent_at: 1, deleted: false };
    link.emit({ ev: "chat.sealed", d: row });
    link.emit({ ev: "chat.sealed.removed", d: { conversation: "dm:x:y", ids: [1] } });
    expect(order).toEqual([]);
    ready(); await c.sealedReady; await Promise.resolve();
    expect(order).toEqual(["receive", "remove"]);
  } finally { c.sealed = null; c.sealedReady = Promise.resolve(); }
});

it("a teardown that resumes after a newer connect leaves that connection alone", async () => {
  connectFake();
  const c = controller as unknown as { conn: unknown; connections: number; runtime: unknown; teardown(): Promise<void> };
  let finish!: () => void;
  c.runtime = { close: () => new Promise<void>((resolve) => { finish = resolve; }) };
  const stale = c.teardown();
  const newer = {};
  c.conn = newer; c.connections++;
  finish(); await stale;
  expect(c.conn).toBe(newer);
  expect(session().me).not.toBeNull();
  c.conn = link;
});

it("a teardown still cleans up when a failed connect clears the connection meanwhile", async () => {
  connectFake();
  const c = controller as unknown as { conn: unknown; runtime: unknown; teardown(): Promise<void> };
  let finish!: () => void;
  c.runtime = { close: () => new Promise<void>((resolve) => { finish = resolve; }) };
  const stale = c.teardown();
  c.conn = null;
  finish(); await stale;
  expect(session().me).toBeNull();
  c.conn = link;
});

it("drops plaintext Gwar-to-Gwar DMs before state or notifications, retaining TeamSpeak DMs", () => {
  connectFake();
  const msg = { id: 700, target: { client: 1 }, author: 2, author_uid: "u2", author_name: "Peer", text: "plaintext", sent_at: 1 };
  link.emit({ ev: "chat.message", d: msg });
  link.emit({ ev: "chat.edited", d: msg });
  expect(session().threads["dm:u2"]).toBeUndefined();
  expect(notifyMock).not.toHaveBeenCalled();
  link.emit({ ev: "chat.message", d: { ...msg, author_uid: "ts:peer" } });
  expect(session().threads["dm:ts:peer"]?.items).toHaveLength(1);
});

describe.each(["channel", "server"] as const)("encrypted %s chat", kind => {
  const thread = kind === "server" ? "server" : "ch:1", target = kind === "server" ? "server" : "channel:1";
  const connectPublic = () => { connectFake({ encrypted: [1], serverChat: kind === "server" }); if (kind === "server") session().dispatch({ type: "setActive", key: "server" }); };
  const sealedRow = (id: number) => ({ id, conversation: target, group: "g", epoch: 0, author_uid: "u2", author_name: "n2",
    message_id: `m${id}`, revision: 0, ciphertext: "x", sent_at: 1000 + id, deleted: false });
  const fakeSealed = () => ({ open: vi.fn(async () => {}), send: vi.fn(async () => {}), close: vi.fn() });
  const install = (sealed: unknown) => { (controller as unknown as { sealed: unknown }).sealed = sealed; };
  beforeEach(() => session().dispatch({ type: "focus", focused: true }));
  afterEach(() => install(null));

  it("reads an encrypted channel through sealed history only", async () => {
    connectPublic();
    const sealed = fakeSealed(); install(sealed);
    // Any change after the runtime is up opens what is on screen.
    session().dispatch({ type: "focus", focused: false });
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("chat.history").filter(r => (r.d as { channel: number }).channel === 1)).toEqual([]);
    expect(sealed.open).toHaveBeenCalledWith(thread);
  });

  it("sends sealed and asks moderators' removals by conversation", async () => {
    connectPublic();
    const sealed = fakeSealed(); install(sealed);
    expect(await controller.sendChat(kind === "server" ? "server" : "channel", "hi", { mentions: ["u2"] })).toBe(true);
    expect(sealed.send).toHaveBeenCalledWith(thread, "hi", ["u2"], undefined, []);
    expect(link.sent("chat.send")).toEqual([]);
    const row = sealedRow(5);
    session().dispatch({ type: "sealed", thread, live: false,
      item: { kind: "msg", key: "e5", at: 0, msg: { ...message(5, 1, { target: kind === "server" ? "server" : { channel: 1 } }), signature: null }, sealed: row, authentic: true } });
    expect(await controller.deleteMessage(5)).toBe(true);
    expect(link.sent("chat.sealed.remove")).toEqual([{ op: "chat.sealed.remove", d: { conversation: target, id: 5 } }]);
    expect(link.sent("chat.delete")).toEqual([]);
    await controller.loadOlder();
    expect(sealed.open).toHaveBeenCalledWith(thread, 5);
  });

  it("sends only this connection's encrypted uploads with a sealed message, and keeps them on edits", async () => {
    connectPublic();
    const sealed = fakeSealed(); install(sealed);
    const uploaded = (controller as unknown as { uploaded: Map<string, { conn: unknown; att: Attachment }> }).uploaded;
    const att = { id: "f1", name: "a.png", mime: "image/png", size: 5, url: "/files/f1/file", sha256: "s", sealed: { key: "k", sha256: "c", size: 33 } };
    uploaded.set("plain", { conn: link, att: { ...att, id: "plain", sealed: undefined } });
    uploaded.set("other", { conn: {}, att: { ...att, id: "other" } });
    for (const id of ["plain", "other", "missing"]) expect(await controller.sendChat(kind === "server" ? "server" : "channel", "hi", { attachments: [id] })).toBe(false);
    expect(sealed.send).not.toHaveBeenCalled();
    uploaded.set("f1", { conn: link, att });
    expect(await controller.sendChat(kind === "server" ? "server" : "channel", "hi", { attachments: ["f1"] })).toBe(true);
    expect(sealed.send).toHaveBeenCalledWith(thread, "hi", [], undefined, [att]);
    expect(uploaded.has("f1")).toBe(false);
  });

  it("removes the sealed row it started with when the view moves meanwhile", async () => {
    connectPublic();
    install(fakeSealed());
    session().dispatch({ type: "sealed", thread, live: false,
      item: { kind: "msg", key: "e6", at: 0, msg: { ...message(6, 1, { target: kind === "server" ? "server" : { channel: 1 } }), signature: null }, sealed: sealedRow(6), authentic: true } });
    const removal = controller.deleteMessage(6);
    session().dispatch({ type: "selectChannel", channel: 2 });
    expect(await removal).toBe(true);
    expect(link.sent("chat.sealed.remove")).toEqual([{ op: "chat.sealed.remove", d: { conversation: target, id: 6 } }]);
    expect(link.sent("chat.delete")).toEqual([]);
  });

  it("reports reading by conversation", async () => {
    connectPublic();
    session().dispatch({ type: "sealed", thread, live: true,
      item: { kind: "msg", key: "e7", at: 0, msg: message(7, 1), sealed: sealedRow(7), authentic: true } });
    await vi.advanceTimersByTimeAsync(1001);
    expect(link.sent("chat.read")).toEqual([{ op: "chat.read", d: { conversation: target, message: 7 } }]);
  });

  it("counts encrypted channels' mentions again after a resync", async () => {
    connectPublic();
    const order: string[] = [];
    const epoch = session().epoch;
    // Reset before the Welcome: its view change recounts at once.
    const sealed = { ...fakeSealed(), reset: vi.fn(() => order.push(`reset ${session().epoch - epoch}`)), refresh: vi.fn(), scanUnread: vi.fn(async () => { order.push(`scan ${session().epoch - epoch}`); }) }; install(sealed);
    link.welcomeNow(welcome({ session: 1, uid: "u1", server: { ...welcome().server, server_chat: { encrypted: kind === "server", history: kind === "server" } }, channels: [chan(1, { encrypted: true })], clients: [client(1, null)] }), true);
    expect(sealed.refresh).toHaveBeenCalled();
    expect(order).toEqual(["reset 0", "scan 1"]);
  });

  it("lets the creator make an encrypted channel's group right away", async () => {
    connectFake();
    const runtime = { channelCreated: vi.fn() };
    (controller as unknown as { runtime: unknown }).runtime = runtime;
    link.request = (async (op: string, d: unknown) => { link.requests.push({ op, d }); return chan(9, { encrypted: true }); }) as never;
    await controller.createChannel({ name: "secret", encrypted: true });
    expect(runtime.channelCreated).toHaveBeenCalledWith(9);
    (controller as unknown as { runtime: unknown }).runtime = null;
  });
});


it.each(["gwar", "teamspeak"] as const)("keeps default server chat plaintext in %s sessions", async kind => {
  connectFake({ kind });
  expect(await controller.sendChat("server", "plain server hello")).toBe(true);
  expect(link.sent("chat.send").at(-1)?.d).toMatchObject({ target: "server", text: "plain server hello", signature: expect.any(Object) });
  expect(link.sent("chat.sealed.send")).toEqual([]);
});

describe("accepting a changed server key", () => {
  it("keeps the old pin until the new key is proven, so no third key is trusted on first use meanwhile", async () => {
    vi.useRealTimers();
    const endpoint = "wss://changed.example/ws", key = "gwar.server-pin:" + endpoint;
    const [old, accepted, third] = ["A".repeat(43), "B".repeat(42) + "A", "C".repeat(42) + "A"];
    const store = new Map([[key, JSON.stringify({ server_pk: old, first_seen: 1 })]]);
    // The device's security state, shared with "another tab" (serverPins below runs the same transactions on it).
    const { localState } = await import("../lib/local-state");
    const get = vi.spyOn(localState, "get").mockImplementation((k) => store.get(k));
    const update = vi.spyOn(localState, "update").mockImplementation(async (k, change) => {
      const next = change(store.get(k));
      if (next === null) store.delete(k); else if (next !== undefined) store.set(k, next);
      return store.get(k);
    });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const c = controller as any;
    const connect = vi.spyOn(c, "connectInteractive").mockImplementation(async () => true);
    try {
      const { ServerIdentityChanged, serverPins } = await import("../net/server-pins");
      c.confirmServerChange(new ServerIdentityChanged(endpoint, old, accepted, "pin"), { address: "changed.example" }, { remember: false });
      (stores.useUi.getState().dialog as { onConfirm(): void }).onConfirm();
      await new Promise((resolve) => setTimeout(resolve, 0));
      // The reconnect to the accepted key is under way: the address still has its pin, so another tab's first
      // contact with a third key is refused.
      expect(JSON.parse(store.get(key) ?? "null")?.server_pk).toBe(old);
      await expect(serverPins.commit(endpoint, third)).rejects.toBeInstanceOf(ServerIdentityChanged);
      expect(connect).toHaveBeenCalledWith(expect.objectContaining({ inviteServerKey: accepted, replaceServerKey: old }), { remember: false });
    } finally { get.mockRestore(); update.mockRestore(); connect.mockRestore(); }
  });
});
