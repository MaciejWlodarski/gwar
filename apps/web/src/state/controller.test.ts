import { notificationServerKey } from "../lib/notification-scope";
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
import type { VoiceEngine, VoiceEvents, VoiceState } from "../voice/engine";
import { defaultSounds } from "../sound/catalog";

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
vi.mock("../lib/official", async get => ({ ...await get<typeof import("../lib/official")>(), CONNECT_URL: "http://127.0.0.1:18980" }));
const notifyMock = vi.hoisted(() => vi.fn());
vi.mock("../platform/notify", () => ({ notify: notifyMock }));

// The controller is the browser glue; give it just enough of a browser.
type Ctl = typeof import("./controller").controller;
type Stores = typeof import("./stores");
let controller: Ctl;
let stores: Stores;
let settings: typeof import("./settings");
const windowEvents = new EventTarget();
let engineState: (state: import("../voice/engine").VoiceState) => void;

class FakeLink implements Link {
  status: ConnectionStatus = { state: "online" };
  isOnline = true;
  requests: Array<{ op: string; d: unknown }> = [];
  private welcomeHandlers: Array<(w: Welcome, resync: boolean) => void> = [];
  private eventHandlers: Array<(e: Event) => void> = [];
  private statusHandlers: Array<(s: ConnectionStatus) => void> = [];
  failing = new Set<string>();
  private rttHandlers: Array<(ms: number) => void> = [];
  onRtt(h: (ms: number) => void) { this.rttHandlers.push(h); return () => {}; }
  rtt(ms: number) { for (const h of this.rttHandlers) h(ms); }

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

const engineHandlers = new Map<string, (value: unknown) => void>();
function fakeEngine() {
  const handlers = new Map<string, Set<(value: never) => void>>();
  const engine = {
    playSound: vi.fn(),
    emitState: (state: VoiceState) => { for (const handler of handlers.get("state") ?? []) handler(state as never); },
    capabilities: { outputDeviceSelection: false, encryptedVoice: async () => true },
    start: vi.fn(async (_options: Parameters<VoiceEngine["start"]>[0]) => {}),
    stop: vi.fn(async () => {}),
    retryMic: vi.fn(async () => {}),
    setMuted: vi.fn(),
    setDeafened: vi.fn(),
    setInputMode: vi.fn(),
    setInputSensitivity: vi.fn(),
    setPttActive: vi.fn(),
    setPttReleaseDelay: vi.fn(),
    setInputDevice: vi.fn(async () => {}),
    setOutputDevice: vi.fn(async () => {}),
    setCaptureOptions: vi.fn(async () => {}),
    setMasterVolume: vi.fn(),
    setUserVolume: vi.fn(),
    setSlotOwner: vi.fn(),
    listDevices: vi.fn(async () => ({ inputs: [], outputs: [] })),
    startMicTest: vi.fn(async () => () => {}),
    micPermission: vi.fn(async () => "granted" as const),
    on: vi.fn(<E extends keyof VoiceEvents>(event: E, handler: (value: VoiceEvents[E]) => void) => {
      const listeners = handlers.get(event) ?? new Set();
      handlers.set(event, listeners); listeners.add(handler as (value: never) => void);
      engineHandlers.set(event, handler as (value: unknown) => void);
      return () => { listeners.delete(handler as (value: never) => void); };
    }),
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
  vi.stubGlobal("window", { addEventListener: windowEvents.addEventListener.bind(windowEvents), removeEventListener: windowEvents.removeEventListener.bind(windowEvents), location: { protocol: "http:", hostname: "localhost", port: "5173" } });
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
  controller.sounds.reset();
  settings.useSettings.setState({ sounds: defaultSounds, notificationOverrides: { servers: {}, channels: {} } });
  settings.useSettings.setState({ onboarding: { completed: true, firstUsedAt: 1, nudgeDismissed: false, roleReceived: false } });
  engine.capabilities.encryptedVoice = async () => true;
  link = new FakeLink();
  session().dispatch({ type: "reset" });
  stores.useUi.setState({ dialog: { kind: "none" }, joining: null, toasts: [] });
  stores.useVoice.setState({ muted: false, deafened: false, micError: null, state: "idle", reconnecting: false, failure: null });
  settings.useSettings.setState({ selfMuted: false, selfDeafened: false, resumeConnection: null, updateVoiceResume: null, language: "en" });
  stores.useConnectUi.setState({ restoring: false, startupRetry: false, otherTab: null, busy: false, error: null, invite: null, inviteLaunch: false });
  (controller as unknown as { restored: boolean; persistRequested: boolean }).restored = false;
  (controller as unknown as { persistRequested: boolean }).persistRequested = false;
  (controller as unknown as { voiceAutoStart: boolean; rememberResume: boolean; voiceRetryAllowed: boolean }).voiceAutoStart = true;
  (controller as unknown as { rememberResume: boolean; voiceRetryAllowed: boolean }).rememberResume = true;
  (controller as unknown as { voiceRetryAllowed: boolean }).voiceRetryAllowed = true;
  engine.start.mockReset().mockResolvedValue(undefined);
});

afterEach(async () => {
  vi.useRealTimers();
  await controller.disconnect(true);
});

describe("sound cue wiring", () => {
  it("confirms connected voice and leaving, but never a mere server welcome", async () => {
    connectFake({ channel: 2 });
    expect(engine.playSound).not.toHaveBeenCalled();
    engine.emitState("connected");
    expect(engine.playSound).toHaveBeenLastCalledWith("selfJoin", 0.6);
    await vi.advanceTimersByTimeAsync(1000);
    await controller.leaveVoice();
    expect(engine.playSound).toHaveBeenLastCalledWith("selfLeave", 0.6);
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "selfLeave"]);
  });

  it("reports a media loss once and confirms its recovery", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed"); engine.emitState("failed");
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "reconnected"]);
  });

  it("keeps one loss/recovery pair across media failure, signalling retries and a new session", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed");
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(10);
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    link.emit({ ev: "client.updated", d: client(9, 2, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected"); engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "reconnected"]);
  });

  it("never confirms leaving or joining when transport recovery moves presence out and back", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    link.emit({ ev: "voice.closed", d: { call: client(1, 2).voice_binding!.call, participation: client(1, 2).voice_binding!.participation, run: 1, reason: "transport_failed" } });
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(1000);
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "reconnected"]);
  });

  it("does not play an old terminal loss after a newer connection attempt began", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    const c = controller as unknown as { runtime: unknown; conn: Link | null; connectAttempt: number };
    let finish!: () => void;
    c.runtime = { close: () => new Promise<void>(resolve => { finish = resolve; }) };
    link.status_({ state: "closed", reason: { kind: "server", reason: { kind: "replaced" } } });
    c.conn = null; c.connectAttempt++;
    finish(); await vi.advanceTimersByTimeAsync(0);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin"]);
  });

  it("S1 keeps a forced move into unsupported encrypted voice silent", async () => {
    connectFake({ channel: 1, encrypted: [2] }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.capabilities.encryptedVoice = async () => false;
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: "Update your browser or use the desktop app to join encrypted voice." });
    expect(controller.sounds.awaitingRecovery).toBe(false);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin"]);
  });

  it.each([false, true])("S2 abandons recovery when explicitly leaving during signalling reconnect (Welcome received = %s)", async welcomed => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    if (welcomed) {
      link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
      await vi.advanceTimersByTimeAsync(1000);
    }
    await controller.leaveVoice();
    expect(controller.sounds.awaitingRecovery).toBe(false);
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(1000);
    await controller.joinChannelInteractive(1);
    link.emit({ ev: "client.updated", d: client(9, 1, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfLeave", "selfJoin"]);
  });

  it.each(["channel.deleted", "voice.closed"] as const)("S2 retires recovery on terminal %s before joining another channel", async terminal => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed");
    if (terminal === "channel.deleted") link.emit({ ev: terminal, d: { channel: 2 } });
    else link.emit({ ev: terminal, d: { ...client(1, 2).voice_binding!, reason: "channel_deleted" } });
    await vi.advanceTimersByTimeAsync(1000);
    expect(controller.sounds.awaitingRecovery).toBe(false);
    await controller.joinChannelInteractive(1);
    link.emit({ ev: "client.updated", d: client(1, 1) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfJoin"]);
  });

  it("S3 keeps certificate renewal silent when voice returns to its channel", async () => {
    const { Connection } = await import("../net/connection");
    const identity = (cert: string) => ({ nickname: "me", publicKey: "k", device: { certificate: cert } }) as unknown as import("../net/identity").Identity;
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    const c = controller as unknown as { conn: Link; connectionParams: import("./controller").ConnectParams };
    c.conn = Object.create(Connection.prototype, { identity: { value: identity("old") }, close: { value: () => {} } }) as Link;
    c.connectionParams = { address: "test" };
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => { connectFake(); });
    try {
      await controller.replaceIdentityQuietly(identity("renewed"));
      link.emit({ ev: "client.updated", d: client(1, 2) });
      await vi.advanceTimersByTimeAsync(1000);
      engine.emitState("connected");
      expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin"]);
    } finally { connect.mockRestore(); }
  });

  it.each([false, true])("S3 retires a failed renewal without clearing a newer participation (newer connection = %s)", async newer => {
    const { Connection } = await import("../net/connection");
    const identity = (cert: string) => ({ nickname: "me", publicKey: "k", device: { certificate: cert } }) as unknown as import("../net/identity").Identity;
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    const c = controller as unknown as { conn: Link; connectionParams: import("./controller").ConnectParams; connectAttempt: number };
    c.conn = Object.create(Connection.prototype, { identity: { value: identity("old") }, close: { value: () => {} } }) as Link;
    c.connectionParams = { address: "test" };
    let reject!: (error: Error) => void;
    const getIdentity = vi.spyOn(controller, "getIdentity").mockImplementationOnce(() => new Promise((_resolve, no) => { reject = no; }));
    try {
      const renewal = controller.replaceIdentityQuietly(identity("renewed"));
      const failed = expect(renewal).rejects.toThrow();
      await vi.advanceTimersByTimeAsync(0);
      expect(getIdentity).toHaveBeenCalledOnce(); // The real connect passed its internal teardown; no socket was opened.
      if (newer) {
        c.connectAttempt++;
        connectFake({ channel: 1 }); engine.emitState("connected");
        await vi.advanceTimersByTimeAsync(1000);
      }
      reject(new Error("identity unavailable")); await failed;
      expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(newer ? ["selfJoin", "selfJoin"] : ["selfJoin"]);
      if (!newer) { connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(1000); }
      engine.emitState("connected");
      expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "selfJoin"]);
    } finally { getIdentity.mockRestore(); }
  });

  it.each(["negotiation", "media", "signalling"] as const)("S5 reports real %s loss during an accepted move before the new channel connects", async failure => {
    connectFake({ channel: 1 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    if (failure === "negotiation") {
      const { VoiceError } = await import("../voice/engine");
      engine.start.mockRejectedValueOnce(new VoiceError("negotiation", "transport failed"));
    }
    await controller.joinChannelInteractive(2);
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(0);
    if (failure === "media") { engine.emitState("failed"); engine.emitState("failed"); }
    if (failure === "signalling") {
      link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
      link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 });
    }
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost"]);
    if (failure === "signalling") {
      link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
      link.emit({ ev: "client.updated", d: client(9, 2, { uid: "u1" }) });
    }
    await vi.advanceTimersByTimeAsync(3000);
    engine.emitState("connected");
    // B has never connected before: this confirms joining B, without reviving recovery of A.
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfJoin"]);
  });

  it.each([
    { phase: "reconnecting", channel: 1 }, { phase: "reconnecting", channel: 2 },
    { phase: "outside voice", channel: 1 }, { phase: "accepted leave", channel: 1 },
  ])("S6 fences a late Leave completion against a newer participation ($phase, channel $channel)", async ({ phase, channel }) => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    if (phase === "reconnecting") link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    if (phase === "outside voice") {
      link.emit({ ev: "voice.closed", d: { ...client(1, 2).voice_binding!, reason: "transport_failed" } });
      link.emit({ ev: "client.updated", d: client(1, null) });
    }
    await vi.advanceTimersByTimeAsync(1000);
    let finish!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(0);
    if (phase === "reconnecting") link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
    await controller.joinChannelInteractive(channel);
    link.emit({ ev: "client.updated", d: client(phase === "reconnecting" ? 9 : 1, channel, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    finish(); await leaving;
    const expected = phase === "accepted leave" ? ["selfJoin", "selfJoin"] : ["selfJoin", "connectionLost", "selfJoin"];
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(expected);
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual([...expected, "connectionLost"]);
  });

  it("S5 keeps B's pending transport feedback when A is deleted after the move", async () => {
    connectFake({ channel: 1 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    link.emit({ ev: "voice.closed", d: { ...client(1, 1).voice_binding!, reason: "transport_failed" } });
    await vi.advanceTimersByTimeAsync(1000);
    await controller.joinChannelInteractive(2);
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(0);
    link.emit({ ev: "channel.deleted", d: { channel: 1 } });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "connectionLost"]);
  });

  it.each([false, true])("S6 handles a late Leave acknowledgement without stopping/resetting a newer channel (newer = %s)", async newer => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    let acknowledge!: (value: never) => void;
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => {
      if (op !== "channel.leave") return request(op, d);
      return new Promise(resolve => { acknowledge = resolve; });
    });
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(0);
    if (newer) {
      await controller.joinChannelInteractive(1);
      link.emit({ ev: "client.updated", d: client(1, 1) });
      await vi.advanceTimersByTimeAsync(1000);
      engine.emitState("connected");
    } else {
      link.emit({ ev: "client.updated", d: client(1, null) });
      await vi.advanceTimersByTimeAsync(1000);
    }
    await vi.advanceTimersByTimeAsync(1000);
    const stops = engine.stop.mock.calls.length;
    acknowledge({} as never); await leaving;
    expect(engine.stop).toHaveBeenCalledTimes(stops);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(newer ? ["selfJoin", "selfJoin"] : ["selfJoin", "selfLeave"]);
    if (newer) {
      await vi.advanceTimersByTimeAsync(1000);
      engine.emitState("failed");
      expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "selfJoin", "connectionLost"]);
    }
  });

  it("keeps an accepted Leave authoritative when the same participation recovers before its acknowledgement", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("failed");
    let acknowledge!: (value: never) => void;
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => op === "channel.leave" ? new Promise(resolve => { acknowledge = resolve; }) : request(op, d));
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected"); // Same-peer transport recovery, not a new channel or voice entry.
    const stops = engine.stop.mock.calls.length;
    acknowledge({} as never); await leaving;
    expect(engine.stop).toHaveBeenCalledTimes(stops + 1);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "reconnected", "selfLeave"]);
  });

  it.each(["password", "refused"])("R4 S1 retires an accepted Leave after a failed move despite a %s join attempt", async attempt => {
    const { VoiceError } = await import("../voice/engine");
    connectFake({ channel: 1 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.start.mockRejectedValueOnce(new VoiceError("negotiation", "B transport failed"));
    await controller.joinChannelInteractive(2);
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(1000);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost"]);
    let acknowledge!: (value: never) => void;
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => op === "channel.leave" ? new Promise(resolve => { acknowledge = resolve; }) : request(op, d));
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(0);
    if (attempt === "password") {
      await controller.joinChannelInteractive(3);
      expect(stores.useUi.getState().dialog).toMatchObject({ kind: "channelPassword", channel: 3 });
    } else {
      link.failing.add("channel.join");
      await controller.joinChannelInteractive(1);
      link.failing.delete("channel.join");
    }
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(0);
    acknowledge({} as never); await leaving;
    expect(controller.sounds.awaitingRecovery).toBe(false);
    await vi.advanceTimersByTimeAsync(1000);
    engine.start.mockRejectedValueOnce(new VoiceError("negotiation", "fresh first connection failed"));
    await controller.joinChannelInteractive(1);
    link.emit({ ev: "client.updated", d: client(1, 1) });
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost"]);
  });

  it("does not let the stop of a server removal confirm a leave over the rejoined call", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    let finish!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(0);
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(1000);
    engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    finish();
    await vi.advanceTimersByTimeAsync(1000);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "selfJoin"]);
    engine.emitState("failed");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "selfJoin", "connectionLost"]);
  });

  it("R4 S2 confirms Leave after a redundant reconnect stop without newer participation", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    let finish!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(0);
    link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    finish(); await leaving;
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfLeave"]);
    link.status_({ state: "reconnecting", attempt: 3, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfLeave"]);
  });

  it.each([false, true])("a redundant stop during a move neither starts voice offline nor duplicates the restored start (Welcome received = %s)", async welcomed => {
    connectFake({ channel: 1 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    let finish!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve; }));
    link.emit({ ev: "client.updated", d: client(1, 2) });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    const restore = async () => {
      link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
      link.emit({ ev: "client.updated", d: client(9, 2, { uid: "u1" }) });
      await vi.advanceTimersByTimeAsync(0);
    };
    if (welcomed) await restore();
    const starts = engine.start.mock.calls.length;
    finish(); await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledTimes(starts);
    if (!welcomed) await restore();
    expect(engine.start).toHaveBeenCalledTimes(2);
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "selfJoin"]);
  });

  it("a stale move completion cannot restart media while the same participation is leaving", async () => {
    connectFake({ channel: 1 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    let finishMove!: () => void, finishLeave!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finishMove = resolve; }));
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finishLeave = resolve; }));
    link.emit({ ev: "client.updated", d: client(1, 2) });
    const leaving = controller.leaveVoice();
    await vi.advanceTimersByTimeAsync(0);
    finishMove(); await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledOnce();
    finishLeave(); await leaving;
    expect(controller.sounds.participation).toBeNull();
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin"]);
  });

  it("confirms a participation's Leave when the first stop completes despite another pending Leave stop", async () => {
    connectFake({ channel: 2 }); engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(1000);
    let finishFirst!: () => void, finishSecond!: () => void;
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finishFirst = resolve; }));
    engine.stop.mockImplementationOnce(() => new Promise<void>(resolve => { finishSecond = resolve; }));
    const first = controller.leaveVoice(), second = controller.leaveVoice();
    finishFirst(); await first;
    const heard = engine.playSound.mock.calls.map(([cue]) => cue);
    finishSecond(); await second;
    expect(heard).toEqual(["selfJoin", "connectionLost", "selfLeave"]);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(heard);
  });

  it("confirms mute and deafen changes, including control cues while deafened", async () => {
    controller.setDeafened(true);
    await vi.advanceTimersByTimeAsync(150);
    controller.setMuted(true);
    await vi.advanceTimersByTimeAsync(150);
    controller.setMuted(false); // also undeafens, with one confirmation
    await vi.advanceTimersByTimeAsync(150);
    controller.setDeafened(true);
    await vi.advanceTimersByTimeAsync(150);
    controller.setDeafened(false);
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["deafen", "mute", "unmute", "deafen", "undeafen"]);
  });
});

describe("session resilience", () => {
  const saved = { address: "test", kind: "gwar" as const, name: "Friends", password: "secret", insecureLan: true };
  let identity: { mockRestore(): void };
  beforeEach(() => { identity = vi.spyOn(controller, "getIdentity").mockResolvedValue({ publicKey: "device-key", nickname: "me" } as import("../net/identity").Identity); });
  afterEach(() => { identity.mockRestore(); });

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

  it.each(["mute", "deafen"] as const)("applies live %s and server presence before a quota failure, without throwing", control => {
    connectFake({ channel: 1 });
    const previous = settings.useSettings.persist.getOptions().storage;
    const error = new DOMException("Storage full", "QuotaExceededError");
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const write = vi.fn(() => {
      if (control === "mute") expect(engine.setMuted).toHaveBeenLastCalledWith(true);
      else expect(engine.setDeafened).toHaveBeenLastCalledWith(true);
      expect(stores.useVoice.getState()).toMatchObject({ muted: control === "mute", deafened: control === "deafen" });
      expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: control === "mute", deafened: control === "deafen" });
      throw error;
    });
    settings.useSettings.persist.setOptions({ storage: { getItem: () => null, setItem: write, removeItem: () => {} } });
    try {
      expect(() => control === "mute" ? controller.setMuted(true) : controller.setDeafened(true)).not.toThrow();
      expect(write).toHaveBeenCalledOnce();
      expect(warn).toHaveBeenCalledWith("Could not save voice preferences:", error);
    } finally { settings.useSettings.persist.setOptions({ storage: previous }); warn.mockRestore(); }
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

  it("a malformed invite launch cannot silently restore a saved connection", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    stores.useConnectUi.setState({ inviteLaunch: true });
    const connect = vi.spyOn(controller, "connectInteractive");
    await controller.restoreSession();
    expect(connect).not.toHaveBeenCalled();
    expect(engine.start).not.toHaveBeenCalled();
    stores.useConnectUi.setState({ inviteLaunch: false });
    connect.mockRestore();
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
    expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: true, deafened: true });
    expect(settings.useSettings.getState()).toMatchObject({ selfMuted: false, selfDeafened: false });
    await controller.joinChannelInteractive(1);
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledOnce();
    expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: false, deafened: false });
    connect.mockRestore();
  });

  it("keeps a restored TeamSpeak session silent across server moves and presence changes until explicit voice entry", async () => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => { connectFake({ kind: "teamspeak", channel: 1, params }); return true; });
    try {
      await controller.restoreSession();
      link.emit({ ev: "client.updated", d: client(1, 2) });
      await vi.advanceTimersByTimeAsync(0);
      link.emit({ ev: "client.updated", d: client(1, 2, { nickname: "new name" }) });
      controller.setMuted(false); controller.setDeafened(false);
      link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
      link.welcomeNow(welcome({ session: 1, uid: "u1", channels: [chan(1), chan(2)], clients: [client(1, 1)] }), true);
      await vi.advanceTimersByTimeAsync(0);
      expect(engine.start).not.toHaveBeenCalled();
      expect(link.sent("channel.join")).toHaveLength(0);
      expect(link.sent("screen.start")).toHaveLength(0);
      expect(stores.useVoice.getState().state).toBe("idle");
      expect(link.sent("client.update").every(request => (request.d as { muted: boolean; deafened: boolean }).muted && (request.d as { deafened: boolean }).deafened)).toBe(true);
      await controller.joinChannelInteractive(2);
      link.emit({ ev: "client.updated", d: client(1, 2) });
      await vi.advanceTimersByTimeAsync(0);
      expect(engine.start).toHaveBeenCalledOnce();
      expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: false, deafened: false });
    } finally { connect.mockRestore(); }
  });

  it("a refused explicit join does not authorize capture on a later server move", async () => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => { connectFake({ kind: "teamspeak", channel: 1, params }); return true; });
    try {
      await controller.restoreSession(); link.failing.add("channel.join");
      await expect(controller.joinChannel(2)).rejects.toThrow("boom");
      link.emit({ ev: "client.updated", d: client(1, 2) });
      await vi.advanceTimersByTimeAsync(0);
      expect(engine.start).not.toHaveBeenCalled();
      expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: true, deafened: true });
    } finally { connect.mockRestore(); }
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

  it("removing the connected bookmark prevents a later resync from restoring its credentials", () => {
    settings.useSettings.getState().saveBookmark({ id: "current", address: "test", kind: "gwar", name: "Friends" });
    connectFake({ params: saved });
    expect(settings.useSettings.getState().resumeConnection).not.toBeNull();
    settings.useSettings.getState().removeBookmark("current");
    link.welcomeNow(welcome({ session: 1, uid: "u1", clients: [client(1, null)] }), true);
    expect(settings.useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
  });

  it("removing the restoration bookmark discards the other-tab takeover credentials too", () => {
    settings.useSettings.getState().saveBookmark({ id: "current", ...saved });
    settings.useSettings.setState({ resumeConnection: saved });
    stores.useConnectUi.setState({ otherTab: saved });
    settings.useSettings.getState().removeBookmark("current");
    expect(stores.useConnectUi.getState().otherTab).toBeNull();
    expect(settings.useSettings.getState().resumeConnection).toBeNull();
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

  it.each(["valid", "expired", "other version", "other server", "expired during connect"])("consumes an update voice intent once: %s", async variant => {
    settings.useSettings.setState({ resumeConnection: saved, updateVoiceResume: {
      address: variant === "other server" ? "other.example" : "test", kind: "gwar", channel: 2, password: "channel-secret", version: variant === "other version" ? "old" : __APP_VERSION__, expiresAt: Date.now() + (variant === "expired" ? -1 : 1000),
    } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async () => {
      if (variant === "expired during connect") vi.setSystemTime(Date.now() + 2000);
      connectFake(); return true;
    });
    await controller.restoreSession();
    expect(link.sent("channel.join")).toHaveLength(variant === "valid" ? 1 : 0);
    if (variant === "valid") expect(link.sent("channel.join")[0]!.d).toEqual({ channel: 2, password: "channel-secret" });
    expect(settings.useSettings.getState().updateVoiceResume).toBeNull();
    connect.mockRestore();
  });

  it.each(["valid", "expires during connect", "expires during move", "wrong channel", "cancelled"])("restores TeamSpeak silently and authorizes update capture only at its unexpired target: %s", async variant => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" }, updateVoiceResume: {
      address: "test", kind: "teamspeak", channel: 2, version: __APP_VERSION__, expiresAt: Date.now() + 1000,
    } });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => {
      if (variant === "expires during connect") vi.setSystemTime(Date.now() + 2000);
      connectFake({ kind: "teamspeak", channel: 1, params });
      expect(engine.start).not.toHaveBeenCalled();
      expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: true, deafened: true });
      return true;
    });
    try {
      await controller.restoreSession();
      expect(engine.start).not.toHaveBeenCalled();
      expect(link.sent("channel.join")).toHaveLength(variant === "expires during connect" ? 0 : 1);
      if (variant === "expires during move") vi.setSystemTime(Date.now() + 2000);
      if (variant === "cancelled") controller.clearSessionResume();
      link.emit({ ev: "client.updated", d: client(1, variant === "wrong channel" ? 3 : 2) });
      await vi.advanceTimersByTimeAsync(0);
      expect(engine.start).toHaveBeenCalledTimes(variant === "valid" ? 1 : 0);
      if (variant === "valid") {
        expect(engine.start.mock.calls[0]?.[0]).toMatchObject({ binding: { channel: 2 } });
        expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: false, deafened: false });
      } else expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: true, deafened: true });
    } finally { connect.mockRestore(); }
  });

  it.each([false, true])("revalidates an update already at its target after publishing presence (expired: %s)", async expired => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" }, updateVoiceResume: {
      address: "test", kind: "teamspeak", channel: 1, version: __APP_VERSION__, expiresAt: Date.now() + 1000,
    } });
    const original = link.request.bind(link);
    const request = vi.spyOn(link, "request");
    request.mockImplementation(async (op, data) => {
      const result = await original(op, data);
      if (expired && op === "client.update" && !(data as { muted: boolean }).muted) vi.setSystemTime(Date.now() + 2000);
      return result;
    });
    const connect = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => { connectFake({ kind: "teamspeak", channel: 1, params }); return true; });
    try {
      await controller.restoreSession();
      expect(engine.start).toHaveBeenCalledTimes(expired ? 0 : 1);
      expect(link.sent("channel.join")).toHaveLength(0);
      expect(link.sent("client.update").at(-1)?.d).toEqual({ muted: expired, deafened: expired });
    } finally { connect.mockRestore(); request.mockRestore(); }
  });

  it("does not restore over another tab's device lock, and deliberate Connect here can take over", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const previous = globalThis.navigator;
    const query = vi.fn(async () => ({ held: [{ name: "gwar-mls:device-key" }], pending: [] }));
    vi.stubGlobal("navigator", { locks: { query } });
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => { await controller.disconnect(true); connectFake(); });
    try {
      await controller.restoreSession();
      expect(query).toHaveBeenCalledOnce();
      expect(connect).not.toHaveBeenCalled();
      expect(stores.useConnectUi.getState()).toMatchObject({ restoring: false, otherTab: saved, busy: false });
      await controller.connectInteractive(stores.useConnectUi.getState().otherTab!, { remember: false });
      expect(connect).toHaveBeenCalledOnce();
      expect(stores.useConnectUi.getState().otherTab).toBeNull();
    } finally { connect.mockRestore(); vi.stubGlobal("navigator", previous); }
  });

  it("an unrelated device lock does not prevent restoration", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const previous = globalThis.navigator;
    vi.stubGlobal("navigator", { locks: { query: async () => ({ held: [{ name: "gwar-mls:someone-else" }] }) } });
    const connect = vi.spyOn(controller, "connectInteractive").mockResolvedValue(true);
    try { await controller.restoreSession(); expect(connect).toHaveBeenCalledOnce(); }
    finally { connect.mockRestore(); vi.stubGlobal("navigator", previous); }
  });

  it("does not take over if the lock snapshot cannot be read", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const previous = globalThis.navigator;
    vi.stubGlobal("navigator", { locks: { query: async () => { throw new Error("unavailable"); } } });
    const connect = vi.spyOn(controller, "connectInteractive");
    try { await controller.restoreSession(); expect(connect).not.toHaveBeenCalled(); expect(stores.useConnectUi.getState().otherTab).toEqual(saved); }
    finally { connect.mockRestore(); vi.stubGlobal("navigator", previous); }
  });

  it("cancels a pending ownership query when the user disconnects", async () => {
    settings.useSettings.setState({ resumeConnection: saved });
    const previous = globalThis.navigator;
    let finish!: (value: object) => void;
    vi.stubGlobal("navigator", { locks: { query: () => new Promise(resolve => { finish = resolve; }) } });
    const connect = vi.spyOn(controller, "connectInteractive");
    try {
      const restoring = controller.restoreSession(); await vi.advanceTimersByTimeAsync(0);
      await controller.disconnect(); finish({ held: [] }); await restoring;
      expect(connect).not.toHaveBeenCalled(); expect(stores.useConnectUi.getState().restoring).toBe(false);
    } finally { connect.mockRestore(); vi.stubGlobal("navigator", previous); }
  });

  it.each(["success", "exhausted", "cancel", "online"])("retries an unreachable startup with bounded delays: %s", async outcome => {
    const { ConnectFailure } = await import("./controller");
    settings.useSettings.setState({ resumeConnection: saved });
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => {
      await controller.disconnect(true);
      throw new ConnectFailure("unreachable", "offline");
    });
    try {
      const restoring = controller.restoreSession(); await vi.advanceTimersByTimeAsync(0);
      expect(connect).toHaveBeenCalledOnce();
      expect(stores.useConnectUi.getState()).toMatchObject({ restoring: true, startupRetry: true });
      if (outcome === "cancel") await controller.disconnect();
      else if (outcome === "online" || outcome === "success") {
        connect.mockImplementation(async () => { await controller.disconnect(true); connectFake(); });
        if (outcome === "online") windowEvents.dispatchEvent(new Event("online"));
        else await vi.advanceTimersByTimeAsync(2000);
      } else {
        await vi.advanceTimersByTimeAsync(1999); expect(connect).toHaveBeenCalledOnce();
        await vi.advanceTimersByTimeAsync(1); expect(connect).toHaveBeenCalledTimes(2);
        await vi.advanceTimersByTimeAsync(4999); expect(connect).toHaveBeenCalledTimes(2);
        await vi.advanceTimersByTimeAsync(1);
      }
      await restoring; await vi.advanceTimersByTimeAsync(60_000);
      expect(connect).toHaveBeenCalledTimes(outcome === "cancel" ? 1 : outcome === "exhausted" ? 3 : 2);
      expect(stores.useConnectUi.getState()).toMatchObject({ restoring: false, startupRetry: false });
    } finally { connect.mockRestore(); }
  });

  it("clears server secrets and update intent on identity changes and imports", async () => {
    const record = { address: "test", kind: "gwar" as const, version: __APP_VERSION__, channel: 1, expiresAt: Date.now() + 1000 };
    settings.useSettings.setState({ resumeConnection: saved, updateVoiceResume: record });
    await controller.switchIdentity({ nickname: "new", publicKey: "new-key" } as import("../net/identity").Identity);
    expect(settings.useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
    settings.useSettings.setState({ resumeConnection: saved, updateVoiceResume: record });
    const imported = vi.spyOn(await import("../net/identity"), "importIdentity").mockResolvedValue({ nickname: "imported", publicKey: "imported" } as import("../net/identity").Identity);
    try {
      await controller.importIdentity("backup");
      expect(settings.useSettings.getState()).toMatchObject({ resumeConnection: null, updateVoiceResume: null });
    } finally { imported.mockRestore(); }
  });

  it.each(["unsupported", "insecure"] as const)("does not keep retrying a permanent %s refusal, but manual Retry is available", async kind => {
    const { VoiceError } = await import("../voice/engine");
    engine.start.mockRejectedValue(new VoiceError(kind, "permanent refusal"));
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", reconnecting: false, failure: expect.any(String) });
    expect(stores.useUi.getState().toasts).toHaveLength(0);
    const reason = stores.useVoice.getState().failure;
    link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason: "transport_failed" } });
    expect(stores.useVoice.getState()).toMatchObject({ reconnecting: false, failure: reason });
    await vi.advanceTimersByTimeAsync(120_000); expect(engine.start).toHaveBeenCalledOnce();
    engine.start.mockResolvedValue(undefined); controller.retryVoice(); await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledTimes(2);
  });

  it.each(["gwar", "teamspeak"] as const)("preserves a permanent refusal through %s recovery until explicit Retry rejoins the target", async kind => {
    const { VoiceError } = await import("../voice/engine");
    engine.start.mockRejectedValue(new VoiceError("unsupported", "permanent refusal"));
    connectFake({ channel: 2, kind }); await vi.advanceTimersByTimeAsync(0);
    const reason = stores.useVoice.getState().failure;
    expect(reason).not.toBeNull();
    engine.stop.mockImplementationOnce(async () => { engine.emitState("idle"); });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason, reconnecting: false });
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2), chan(3)], clients: [client(9, kind === "teamspeak" ? 1 : null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(engine.start).toHaveBeenCalledOnce();
    expect(link.sent("channel.join")).toHaveLength(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason, reconnecting: false });
    expect(stores.useUi.getState().toasts).toHaveLength(0);
    engine.start.mockResolvedValue(undefined); controller.retryVoice(); await vi.advanceTimersByTimeAsync(0);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 2, password: null });
    expect(engine.start).toHaveBeenCalledOnce();
    link.emit({ ev: "client.updated", d: client(9, 3, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledOnce();
    expect(stores.useVoice.getState().failure).toBe(reason);
    link.emit({ ev: "client.updated", d: client(9, 2, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledTimes(2);
    expect(stores.useVoice.getState().failure).toBeNull();
  });

  it.each([1, 2])("only explicit channel entry resets a permanent refusal after a server move (target %s)", async target => {
    const { VoiceError } = await import("../voice/engine");
    engine.start.mockRejectedValue(new VoiceError("insecure", "permanent refusal"));
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    const reason = stores.useVoice.getState().failure;
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(0);
    engine.emitState("connected"); // A stale engine callback is not explicit consent to Retry.
    expect(engine.start).toHaveBeenCalledOnce();
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason });
    engine.start.mockResolvedValue(undefined);
    await controller.joinChannelInteractive(target);
    if (target !== 2) link.emit({ ev: "client.updated", d: client(1, target) });
    await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledTimes(2);
    expect(stores.useVoice.getState().failure).toBeNull();
  });

  it.each([false, true])("clears stale voice congestion on a refused engine callback (native %s)", async native => {
    const { VoiceError } = await import("../voice/engine");
    const { useUplink } = await import("./uplink");
    engine.start.mockRejectedValue(new VoiceError("insecure", "permanent refusal"));
    connectFake({ channel: 1 }); await vi.advanceTimersByTimeAsync(0);
    const reason = stores.useVoice.getState().failure;
    expect(reason).not.toBeNull();
    const status = { congested: true, rttMs: 480, baselineMs: 20, availableOutgoingBitrate: null };
    useUplink.setState({ voice: status, screen: null });
    const originalWindow = window;
    const win: { __TAURI_INTERNALS__?: object; location: object } = { location: { protocol: "http:", hostname: "localhost", port: "5173" } };
    if (native) win.__TAURI_INTERNALS__ = {};
    vi.stubGlobal("window", win);
    try {
      engine.emitState("connected");
      expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason });
      expect(useUplink.getState().voice).toBeNull();
      expect(useUplink.getState().notice).toBeNull();
    } finally { vi.stubGlobal("window", originalWindow); }
  });

  it("a browser unable to encrypt stays unavailable, with a reason and no automatic retry", async () => {
    engine.capabilities.encryptedVoice = async () => false;
    connectFake({ channel: 2, encrypted: [2] }); await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", reconnecting: false, failure: "Update your browser or use the desktop app to join encrypted voice." });
    expect(stores.useUi.getState().toasts).toHaveLength(0);
    await vi.advanceTimersByTimeAsync(120_000); expect(engine.start).not.toHaveBeenCalled();
    const reason = stores.useVoice.getState().failure;
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(0);
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(2, { encrypted: true })], clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(120_000);
    expect(link.sent("channel.join")).toHaveLength(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason });
    controller.retryVoice(); await vi.advanceTimersByTimeAsync(0);
    controller.retryVoice(); await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason, reconnecting: false });
    expect(stores.useUi.getState().toasts).toHaveLength(0);
    expect(engine.start).not.toHaveBeenCalled();
  });

  it("a newer permanent refusal cancels a plaintext entry whose presence update is delayed", async () => {
    engine.capabilities.encryptedVoice = async () => false;
    connectFake({ channel: 2, encrypted: [2, 3] }); await vi.advanceTimersByTimeAsync(0);
    const reason = stores.useVoice.getState().failure;
    expect(reason).not.toBeNull();
    // The plaintext join is acknowledged, but its presence update has not arrived yet.
    await controller.joinChannel(1);
    await controller.joinChannel(3, "channel-secret");
    expect(link.sent("channel.join").map(request => request.d)).toEqual([{ channel: 1, password: null }]);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason });
    link.emit({ ev: "client.updated", d: client(1, 1) });
    await vi.advanceTimersByTimeAsync(120_000);
    expect(engine.start).not.toHaveBeenCalled();
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: reason, reconnecting: false });
    expect(stores.useUi.getState().toasts).toHaveLength(0);
    await controller.joinChannelInteractive(1); await vi.advanceTimersByTimeAsync(0);
    expect(engine.start).toHaveBeenCalledOnce();
    expect(stores.useVoice.getState().failure).toBeNull();
  });

  it("announces starting while the encryption capability probe is still pending", async () => {
    let finish!: (value: boolean) => void;
    engine.capabilities.encryptedVoice = () => new Promise(resolve => { finish = resolve; });
    connectFake({ channel: 2, encrypted: [2] });
    expect(stores.useVoice.getState()).toMatchObject({ state: "starting", failure: null });
    finish(false); await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState().state).toBe("failed");
  });

  it("a silent TeamSpeak presence can switch servers without a leave-call confirmation", async () => {
    settings.useSettings.setState({ resumeConnection: { ...saved, kind: "teamspeak" } });
    const restoring = vi.spyOn(controller, "connectInteractive").mockImplementation(async params => { connectFake({ kind: "teamspeak", channel: 1, params }); return true; });
    await controller.restoreSession(); restoring.mockRestore();
    const connect = vi.spyOn(controller, "connect").mockImplementation(async () => { await controller.disconnect(true); });
    try {
      await controller.connectInteractive({ address: "other.example" }, { remember: false });
      expect(connect).toHaveBeenCalledOnce(); expect(stores.useUi.getState().dialog.kind).toBe("none");
    } finally { connect.mockRestore(); }
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


it("refuses an expired device locally before opening a server connection", async () => {
  const get = vi.spyOn(controller, "getIdentity").mockResolvedValue({ nickname: "Alex", publicKey: "key", device: { expires_at: 1 } } as unknown as import("../net/identity").Identity);
  try {
    await expect(controller.connect({ address: "ws://127.0.0.1:8799/ws" })).rejects.toMatchObject({ kind: "certificate_expired" });
    expect(session().phase).toBe("idle");
  } finally { get.mockRestore(); }
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

  it.each(["channel_restarted", "epoch_exhausted", "stream_limit", "state_limit"] as const)("says voice disconnected without claiming to retry after %s", async (reason) => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    engine.start.mockClear();
    link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason } });
    await vi.advanceTimersByTimeAsync(30_000);
    const notice = stores.useUi.getState().toasts.find(t => t.key === "voice-closed")!;
    expect(notice).toMatchObject({ text: "Voice disconnected.", sticky: true, action: { label: "Retry" } });
    expect(engine.start).not.toHaveBeenCalled();
    notice.action!.run();
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).toHaveBeenCalledOnce();
  });

  it("keeps one keyed reconnecting notice beyond five retries, and clears it on recovery", async () => {
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    engine.start.mockClear();
    engine.start.mockImplementation(async () => engine.emitState("failed"));
    try {
      link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason: "transport_failed" } });
      expect(stores.useUi.getState().toasts.find(t => t.key === "voice-closed")?.text).toContain("Reconnecting");
      await vi.advanceTimersByTimeAsync(60_000);
      const notices = stores.useUi.getState().toasts.filter(t => t.key === "voice-closed");
      expect(notices).toHaveLength(1);
      expect(notices[0]).toMatchObject({ kind: "error", text: "The voice connection was closed. Reconnecting…", sticky: true });
      expect(notices[0]!.action).toBeUndefined();
      expect(notices[0]!.details).toBeUndefined();
      expect(engine.start).toHaveBeenCalledTimes(6);
      engine.start.mockImplementation(async () => engine.emitState("connected"));
      controller.retryVoice(); // The panel owns retry-now while recovery is automatic.
      await vi.advanceTimersByTimeAsync(10);
      expect(engine.start).toHaveBeenCalledTimes(7);
      expect(stores.useUi.getState().toasts.some(t => t.key === "voice-closed")).toBe(false);
    } finally { engine.start.mockImplementation(async () => {}); }
  });

  it("S1 rejoins the closed channel on Retry and removes Retry when that channel is removed", async () => {
    const close = (reason: "channel_restarted" | "stream_limit" | "channel_deleted") => link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason } });
    const notice = () => stores.useUi.getState().toasts.find(t => t.key === "voice-closed")!;
    connectFake({ channel: 1 });
    await vi.advanceTimersByTimeAsync(10);
    engine.start.mockClear();
    close("channel_restarted");
    // The real server follows voice.closed with a move outside voice.
    link.emit({ ev: "client.updated", d: client(1, null) });
    await vi.advanceTimersByTimeAsync(10);
    notice().action!.run();
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 1, password: null });
    expect(engine.start).not.toHaveBeenCalled();
    link.emit({ ev: "client.updated", d: client(1, 1) });
    await vi.advanceTimersByTimeAsync(10);
    expect(engine.start).toHaveBeenCalledOnce();
    engine.emitState("connected");
    close("stream_limit");
    link.emit({ ev: "client.updated", d: client(1, null) });
    link.emit({ ev: "channel.deleted", d: { channel: 1 } });
    await vi.advanceTimersByTimeAsync(10);
    expect(notice().text).toBe("The voice channel was removed. Join another channel to talk.");
    expect(notice().action).toBeUndefined();
    link.emit({ ev: "client.updated", d: client(1, 2) });
    await vi.advanceTimersByTimeAsync(10);
    // The reason alone suffices, even before channel.deleted reaches this device.
    close("channel_deleted");
    expect(notice().text).toBe("The voice channel was removed. Join another channel to talk.");
    expect(notice().action).toBeUndefined();
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

  it.each([false, true])("rejoins a sharer's password channel through its override and republishes the retained capture (voice needs retry = %s)", async retry => {
    connectFake({ channel: 2 }); await vi.advanceTimersByTimeAsync(10);
    engine.emitState("connected");
    await vi.advanceTimersByTimeAsync(1000);
    engine.start.mockClear();
    if (retry) {
      const { VoiceError } = await import("../voice/engine");
      engine.start.mockRejectedValueOnce(new VoiceError("negotiation", "temporary loss"));
    }
    const { useScreen } = await import("./screen");
    const source = { readyState: "live", stop: vi.fn(), applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080 }) };
    const stream = { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream;
    useScreen.setState({ own: { phase: "live", attempt: 7, share: "old", stream, surface: "screen", spec: { mode: "smooth", height: 1080, fps: 30 }, viewers: 0, guests: 0, guestLink: false, preview: false } });
    let started!: (value: { share: string }) => void;
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => {
      if (op !== "screen.start") return request(op, d);
      link.requests.push({ op, d });
      return new Promise(resolve => { started = resolve as never; });
    });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    link.welcomeNow(welcome({ session: 9, uid: "u1", permissions: [],
      channels: [chan(1), chan(2, { has_password: true, overrides: [{ user: "u1", allow: ["bypass_password"], deny: [] }] })],
      clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").at(-1)?.d).toEqual({ channel: 2, password: null });
    const binding = { call: "new-call", participation: "new-participation", run: 2 };
    link.emit({ ev: "client.updated", d: client(9, 2, { uid: "u1", voice_binding: binding }) });
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("screen.start").at(-1)?.d).toMatchObject({ call: binding.call, participation: binding.participation });
    expect(engine.start).toHaveBeenCalledOnce();
    expect(useScreen.getState().own?.stream).toBe(stream); expect(source.stop).not.toHaveBeenCalled();
    if (retry) {
      expect(stores.useVoice.getState().reconnecting).toBe(true);
      await vi.advanceTimersByTimeAsync(3000);
      expect(engine.start).toHaveBeenCalledTimes(2);
      expect(useScreen.getState().own?.stream).toBe(stream); expect(source.stop).not.toHaveBeenCalled();
    }
    engine.emitState("connected");
    expect(engine.playSound.mock.calls.map(([cue]) => cue)).toEqual(["selfJoin", "connectionLost", "reconnected"]);
    await controller.screen.stopShare(); started({ share: "late" }); await vi.advanceTimersByTimeAsync(10);
  });

  it.each([false, true])("bounds retained screen recovery when an encrypted voice refusal survives a restored plaintext channel (explicit Retry = %s)", async retry => {
    engine.capabilities.encryptedVoice = async () => false;
    connectFake({ channel: 2, encrypted: [2] }); await vi.advanceTimersByTimeAsync(0);
    const refusal = stores.useVoice.getState().failure;
    expect(refusal).not.toBeNull();
    link.emit({ ev: "client.updated", d: client(1, 1) });
    await vi.advanceTimersByTimeAsync(0);
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: refusal });

    // The user shares after the server moves them to plaintext; voice is still refused.
    const { useScreen } = await import("./screen");
    const source = { readyState: "live", stop: vi.fn(), applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080 }) };
    const stream = { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream;
    useScreen.setState({ own: { phase: "live", attempt: 7, share: "old", stream, surface: "screen", spec: { mode: "smooth", height: 1080, fps: 30 }, viewers: 0, guests: 0, guestLink: false, preview: false } });
    // Hold any unexpected republish before it can construct a real media transport.
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => {
      if (op !== "screen.start") return request(op, d);
      link.requests.push({ op, d }); return new Promise(() => {});
    });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    await vi.advanceTimersByTimeAsync(10_000);
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2, { encrypted: true })], clients: [client(9, null, { uid: "u1" })] }), true);
    link.emit({ ev: "client.updated", d: client(9, 1, { uid: "u1" }) });
    await vi.advanceTimersByTimeAsync(0);
    expect(link.sent("channel.join")).toHaveLength(0);
    expect(link.sent("screen.start")).toHaveLength(0);
    expect(useScreen.getState().own).toMatchObject({ phase: "reconnecting", stream });
    expect(stores.useVoice.getState()).toMatchObject({ state: "failed", failure: refusal });
    if (retry) {
      controller.retryVoice(); await vi.advanceTimersByTimeAsync(0);
      expect(stores.useVoice.getState().failure).toBeNull();
      expect(engine.start).toHaveBeenCalledOnce();
      expect(link.sent("screen.start")).toHaveLength(1);
      expect(link.sent("screen.start").at(-1)?.d).toMatchObject({ call: client(9, 1).voice_binding!.call, participation: client(9, 1).voice_binding!.participation });
      expect(source.stop).not.toHaveBeenCalled();
      await controller.screen.stopShare();
      return;
    }
    await vi.advanceTimersByTimeAsync(34_999);
    expect(source.stop).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(source.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState()).toMatchObject({ own: null, notice: "reconnect" });
    expect(engine.start).not.toHaveBeenCalled();
  });

  it.each([false, true])("Leave cancels pending voice and screen rejoin during reconnect (after Welcome = %s)", async welcomed => {
    connectFake({ channel: 2 }); await vi.advanceTimersByTimeAsync(10);
    const { useScreen } = await import("./screen");
    const source = { readyState: "live", stop: vi.fn() };
    useScreen.setState({ own: { phase: "live", attempt: 7, share: "retained", stream: { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream,
      surface: "screen", spec: { mode: "smooth", height: 1080, fps: 30 }, viewers: 0, guests: 0, guestLink: false, preview: false } });
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    if (welcomed) {
      link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
      await vi.advanceTimersByTimeAsync(10);
      link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 });
    }
    const stop = vi.spyOn(controller.screen, "stopShare");
    await controller.leaveVoice();
    expect(stop).toHaveBeenCalledOnce();
    expect(source.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().own).toBeNull();
    const joins = link.sent("channel.join").length;
    link.status_({ state: "reconnecting", attempt: 3, retryAt: 0 });
    link.welcomeNow(welcome({ session: 10, uid: "u1", channels: [chan(1), chan(2)], clients: [client(10, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join")).toHaveLength(joins);
    expect(link.sent("channel.leave")).toHaveLength(0);
    expect(stores.useUi.getState().toasts).toEqual([]);
    stop.mockRestore();
  });

  it("keeps a pending voice rejoin across a second blip before its channel update", async () => {
    connectFake({ channel: 2 }); await vi.advanceTimersByTimeAsync(10);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 });
    link.welcomeNow(welcome({ session: 10, uid: "u1", channels: [chan(1), chan(2)], clients: [client(10, null, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").map(r => r.d)).toEqual([{ channel: 2, password: null }, { channel: 2, password: null }]);
  });

  it("Leave stops retained capture during online republishing before screen.start answers", async () => {
    connectFake({ channel: 2 }); await vi.advanceTimersByTimeAsync(10);
    const { useScreen } = await import("./screen");
    const source = { readyState: "live", stop: vi.fn(), applyConstraints: vi.fn(async () => {}), getSettings: () => ({ width: 1920, height: 1080 }) };
    useScreen.setState({ own: { phase: "live", attempt: 7, share: "old", stream: { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream,
      surface: "screen", spec: { mode: "smooth", height: 1080, fps: 30 }, viewers: 0, guests: 0, guestLink: false, preview: false } });
    let started!: (value: { share: string }) => void;
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation((op, d) => op === "screen.start" ? new Promise(resolve => { started = resolve as never; }) : request(op, d));
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 });
    link.welcomeNow(welcome({ session: 9, uid: "u1", channels: [chan(1), chan(2)], clients: [client(9, 2, { uid: "u1" })] }), true);
    await vi.advanceTimersByTimeAsync(10);
    expect(useScreen.getState().own).toMatchObject({ phase: "starting", share: null });
    await controller.leaveVoice();
    expect(source.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().own).toBeNull();
    link.emit({ ev: "client.updated", d: client(9, null, { uid: "u1" }) });
    started({ share: "late" }); await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("screen.stop").at(-1)?.d).toEqual({ share: "late" });
    expect(link.sent("screen.publish.offer")).toHaveLength(0);
    expect(stores.useUi.getState().toasts).toEqual([]);
  });

  it.each(["refused", "manual"])("supersedes automatic rejoin intent when %s", async action => {
    connectFake({ channel: 2 }); await vi.advanceTimersByTimeAsync(10);
    const { RequestError } = await import("../net/connection");
    const request = link.request.bind(link);
    vi.spyOn(link, "request").mockImplementation(async (op, d) => {
      const result = await request(op, d);
      if (action === "refused" && op === "channel.join" && (d as { channel: number }).channel === 2) throw new RequestError("forbidden", "refused");
      return result;
    });
    const resync = (id: number) => link.welcomeNow(welcome({ session: id, uid: "u1", channels: [chan(1), chan(2)], clients: [client(id, null, { uid: "u1" })] }), true);
    link.status_({ state: "reconnecting", attempt: 1, retryAt: 0 }); resync(9);
    await vi.advanceTimersByTimeAsync(10);
    if (action === "manual") {
      await controller.joinChannel(1);
      link.emit({ ev: "client.updated", d: client(9, 1, { uid: "u1" }) });
    }
    const before = link.sent("channel.join").length;
    link.status_({ state: "reconnecting", attempt: 2, retryAt: 0 }); resync(10);
    await vi.advanceTimersByTimeAsync(10);
    expect(link.sent("channel.join").slice(before).map(r => r.d)).toEqual(action === "manual" ? [{ channel: 1, password: null }] : []);
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

  it.each([false, true])("gates all interactive entry points before naming (startup still loading: %s)", async loading => {
    const { useAccount } = await import("./account");
    useAccount.setState({ loaded: !loading, account: null });
    settings.useSettings.setState({ onboarding: loading ? null : { completed: false, firstUsedAt: 1, nudgeDismissed: false, roleReceived: false } });
    ui().openDialog({ kind: "addServer" });
    const connect = vi.spyOn(controller, "connect").mockResolvedValue();
    try {
      expect(await controller.connectInteractive({ address: "test" }, { remember: false })).toBe(false);
      expect(connect).not.toHaveBeenCalled();
      expect(ui()).toMatchObject({ section: "servers", dialog: { kind: "none" } });
    } finally { connect.mockRestore(); }
  });

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
    expect(connect).toHaveBeenCalledWith({ address: "test" }, "renew");
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
  it("names a failed channel save and keeps raw bad-request diagnostics out of the main message", async () => {
    const { RequestError } = await import("../net/connection");
    const { describeRequestError } = await import("./controller");
    const error = new RequestError("bad_request", "channel_id: malformed integer");
    expect(describeRequestError(error, "err.channel.save")).toBe("Couldn’t save the channel. Check its settings and try again.");
    expect(describeRequestError(error)).not.toContain("channel_id");
  });
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
  it("honors a mute selected during verification without dropping the verified message", async () => {
    connectFake(); session().dispatch({ type: "focus", focused: false });
    settings.useSettings.getState().setNotifications({ mentions: true, privateMessages: true, allMessages: false });
    let finish!: (result: import("../net/message-signature").MessageVerification) => void;
    const blocked = new Promise<import("../net/message-signature").MessageVerification>(resolve => { finish = resolve; });
    const c = controller as unknown as { verifyIncoming: (msg: ChatMessage) => Promise<import("../net/message-signature").MessageVerification> };
    const verify = vi.spyOn(c, "verifyIncoming").mockReturnValueOnce(blocked);
    try {
      link.emit({ ev: "chat.message", d: message(616, 2, { mentions: ["u1"], signature: {} as NonNullable<ChatMessage["signature"]> }) });
      await vi.advanceTimersByTimeAsync(0);
      settings.useSettings.getState().setNotificationMode(notificationServerKey(session()), 2, "none");
      finish({ status: "verified", continuity: "proven", authorization: [] });
      await vi.advanceTimersByTimeAsync(10);
      expect(notifyMock).not.toHaveBeenCalled();
      expect(engine.playSound).not.toHaveBeenCalled();
      expect(session().threads["ch:2"]?.items.some(i => i.kind === "msg" && i.msg.id === 616 && i.verified)).toBe(true);
    } finally { verify.mockRestore(); }
  });

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

it("a teardown closes the connection details, which describe that connection", async () => {
  connectFake();
  const { useUplink } = await import("./uplink");
  useUplink.setState({ details: true });
  await (controller as unknown as { teardown(): Promise<void> }).teardown();
  expect(useUplink.getState().details).toBe(false);
  (controller as unknown as { conn: unknown }).conn = link;
});

it("S2 terminal teardown clears session notices but a stale teardown preserves a newer session notice", async () => {
  connectFake({ channel: 1 });
  await vi.advanceTimersByTimeAsync(10);
  const ui = stores.useUi.getState();
  link.emit({ ev: "voice.closed", d: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1, reason: "transport_failed" } });
  for (const key of ["voice-output", "screen-stopped", "identity-load"]) ui.toast("error", key, { key, sticky: true });
  link.isOnline = false;
  link.status_({ state: "closed", reason: { kind: "server", reason: { kind: "replaced" } } });
  await vi.advanceTimersByTimeAsync(10);
  expect(session().me).toBeNull();
  expect(stores.useUi.getState().toasts.filter(t => ["voice-closed", "voice-output", "screen-stopped"].includes(t.key ?? ""))).toEqual([]);
  expect(stores.useUi.getState().toasts.some(t => t.key === "identity-load")).toBe(true);

  link = new FakeLink(); connectFake();
  const c = controller as unknown as { connections: number; runtime: unknown; teardown(): Promise<void> };
  let finish!: () => void;
  c.runtime = { close: () => new Promise<void>(resolve => { finish = resolve; }) };
  const stale = c.teardown();
  link = new FakeLink(); connectFake(); c.connections++;
  ui.toast("info", "New session is reconnecting", { key: "voice-closed", sticky: true });
  finish(); await stale;
  expect(stores.useUi.getState().toasts.find(t => t.key === "voice-closed")?.text).toBe("New session is reconnecting");
});

it("S3 clears the identity-load notice only after a successful active load or adoption", async () => {
  const identities = await import("../net/identity");
  const identity = { nickname: "Recovered", publicKey: "key" } as import("../net/identity").Identity;
  await controller.switchIdentity(null);
  const load = vi.spyOn(identities, "loadActiveIdentity").mockRejectedValueOnce(new Error("Storage unavailable")).mockResolvedValueOnce(identity);
  const warn = () => stores.useUi.getState().toast("error", "Identity unavailable", { key: "identity-load", sticky: true });
  const warned = () => stores.useUi.getState().toasts.some(t => t.key === "identity-load");
  try {
    warn();
    await expect(controller.getIdentity()).rejects.toThrow("Storage unavailable");
    expect(warned()).toBe(true);
    await controller.getIdentity();
    expect(warned()).toBe(false);
    warn();
    await controller.switchIdentity(null);
    expect(warned()).toBe(true);
    // Connect sign-in/recovery adopts the validated device identity through this path.
    await controller.switchIdentity(identity);
    expect(warned()).toBe(false);
  } finally { await controller.switchIdentity(null); load.mockRestore(); }
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


describe("server ping seed for WebRTC floors", () => {
  it("tracks the lowest and latest Gwar server ping and resets on welcome and teardown", async () => {
    const { currentServerPing } = await import("../voice/uplink-congestion");
    connectFake();
    expect(currentServerPing()).toBeNull();
    for (const ms of [40, 25, 0, 60]) link.rtt(ms);
    expect(currentServerPing()).toEqual({ floorMs: 25, latestMs: 60, elevated: 0 });
    // A resync is a new session, possibly over another path.
    link.welcomeNow(welcome(), true);
    expect(currentServerPing()).toBeNull();
    link.rtt(30);
    expect(currentServerPing()).toEqual({ floorMs: 30, latestMs: 30, elevated: 0 });
    await controller.disconnect(true);
    expect(currentServerPing()).toBeNull();
    // A closed connection's late pong is ignored.
    link.rtt(10);
    expect(currentServerPing()).toBeNull();
  });

  it("ignores TeamSpeak pings: they do not reach the SFU's host", async () => {
    const { currentServerPing } = await import("../voice/uplink-congestion");
    connectFake({ kind: "teamspeak" });
    link.rtt(30);
    expect(currentServerPing()).toBeNull();
    expect(session().rtt).toBe(30);
  });
});

describe("native voice uplink fallback", () => {
  const voiceState = (state: string) => engineHandlers.get("state")!(state);

  it("waits for a fresh connected ping, resets across calls, and expires without losing the floor", async () => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const { useUplink } = await import("./uplink");
    connectFake();
    await vi.advanceTimersByTimeAsync(10);
    const win: { __TAURI_INTERNALS__?: object; location: object } = { __TAURI_INTERNALS__: {}, location: { protocol: "http:", hostname: "localhost", port: "5173" } };
    vi.stubGlobal("window", win);
    const ping = async (ms: number) => { await vi.advanceTimersByTimeAsync(10000); link.rtt(ms); };
    try {
      link.rtt(20); await ping(20);
      // The upload saturates before the user joins voice.
      await ping(480); await ping(480);
      expect(useUplink.getState().voice).toBeNull();
      await ping(480);
      expect(useUplink.getState().voice).toBeNull();
      voiceState("connected");
      expect(useUplink.getState().voice).toBeNull();
      await ping(480);
      expect(useUplink.getState().voice).toMatchObject({ congested: true, rttMs: 480, baselineMs: 20 });
      voiceState("starting");
      expect(useUplink.getState().voice).toBeNull();
      voiceState("connected");
      expect(useUplink.getState().voice).toBeNull();
      link.rtt(20);
      await ping(480); await ping(480); await ping(480);
      expect(useUplink.getState().voice?.congested).toBe(true);
      await vi.advanceTimersByTimeAsync(25000);
      expect(useUplink.getState().voice).toBeNull();
      // A silent gap (two lost pings) drops the verdict, not the healthy floor.
      await vi.advanceTimersByTimeAsync(5000); link.rtt(480);
      expect(useUplink.getState().voice).toMatchObject({ congested: false, baselineMs: 20 });
      await ping(480); await ping(480);
      expect(useUplink.getState().voice?.congested).toBe(true);
      await controller.disconnect(true);
      link.rtt(900);
      expect(useUplink.getState().voice).toBeNull();
    } finally { delete win.__TAURI_INTERNALS__; voiceState("idle"); }
  });

  it("does not warn for one or two stalled pings", async () => {
    vi.spyOn(performance, "now").mockImplementation(() => Date.now());
    const { useUplink } = await import("./uplink");
    connectFake();
    const win: { __TAURI_INTERNALS__?: object; location: object } = { __TAURI_INTERNALS__: {}, location: { protocol: "http:", hostname: "localhost", port: "5173" } };
    vi.stubGlobal("window", win);
    try {
      voiceState("connected");
      for (const ms of [20, 20, 900, 20, 600, 600, 20]) {
        link.rtt(ms);
        expect(useUplink.getState().voice?.congested).toBe(false);
        await vi.advanceTimersByTimeAsync(10000);
      }
    } finally { delete win.__TAURI_INTERNALS__; voiceState("idle"); }
  });
});
