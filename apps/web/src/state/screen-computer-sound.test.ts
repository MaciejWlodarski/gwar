import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Link } from "../net/connection";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { legacySpec } from "../voice/screen-quality";
import { ScreenController } from "./screen-controller";
import { initialScreen, useScreen } from "./screen";
import { useSettings } from "./settings";
import { useSession, useUi } from "./stores";

const sound = vi.hoisted(() => ({
  instances: [] as Array<{ link: unknown; ended: (state?: string) => void; resuming: (active: boolean) => void; stop: ReturnType<typeof vi.fn>; start: ReturnType<typeof vi.fn> }>,
  result: null as unknown,
  receivers: [] as Array<{ run: number; slots: Array<[number, number]>; gains: Array<[number, number]>; closed: boolean }>,
  receivable: true,
}));
vi.mock("../voice/share-sound", () => ({
  computerSoundSupport: async () => "ok",
  shareSoundReceivable: async () => sound.receivable,
  ComputerSound: class {
    stop = vi.fn();
    start = vi.fn(async () => sound.result);
    constructor(readonly link: unknown, readonly ended: (state?: string) => void, readonly resuming: (active: boolean) => void) { sound.instances.push(this); }
  },
  ShareSoundReceiver: class {
    slots: Array<[number, number]> = [];
    gains: Array<[number, number]> = [];
    closed = false;
    constructor(_link: unknown, readonly run: number) { sound.receivers.push(this); }
    transform = () => undefined;
    async openSlot(slot: number, gain: number) { this.slots.push([slot, gain]); return true; }
    closeSlot = vi.fn();
    setGain(slot: number, gain: number) { this.gains.push([slot, gain]); }
    close() { this.closed = true; }
  },
}));
const media = vi.hoisted(() => ({ publishers: [] as Array<{ params: { audio?: MediaStreamTrack; nativeAudio?: boolean } }>, transports: [] as Array<{ params: { audio?(): Promise<boolean> } }> }));
vi.mock("../voice/screen", async importOriginal => ({
  ...await importOriginal<typeof import("../voice/screen")>(),
  ScreenPublisher: class {
    fallbacks = vi.fn(); close = vi.fn(); setSpec = vi.fn(async () => {});
    constructor(readonly params: { audio?: MediaStreamTrack; nativeAudio?: boolean }) { media.publishers.push(this); }
    async start() {}
  },
  ScreenWatchTransport: class {
    run: number; close = vi.fn();
    constructor(readonly params: { run: number; audio?(): Promise<boolean> }) { this.run = params.run; media.transports.push(this); }
    async start() {}
  },
  applyCaptureSpec: vi.fn(async () => {}),
  captureSupport: () => "ok" as const,
}));

const ch = (id: number): Channel => ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted: false, history: false, private: false, synced: false });
const cl = (id: number, channel: number | null): Client => ({ id, uid: `u${id}`, nickname: `n${id}`, channel, groups: [2], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: false, voice_binding: { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1 } });

function link(audio = true) {
  return { request: vi.fn(async (op: string) => {
    if (op === "screen.audio.capabilities") return { screen_audio: audio };
    if (op === "screen.spec.capabilities") return { screen_spec: false };
    if (op === "screen.capabilities") return { screen_h264: false };
    if (op === "screen.start") return { share: "M" };
    return {};
  }) } as unknown as Link;
}
const liveTrack = () => ({ readyState: "live", kind: "audio", stop: vi.fn(), addEventListener: vi.fn() }) as unknown as MediaStreamTrack;

function choosing(computerSound: "ok" | "permission" | undefined) {
  const video = { readyState: "live", kind: "video", stop: vi.fn(), getSettings: () => ({ displaySurface: "monitor" }), addEventListener: vi.fn() };
  const tracks: unknown[] = [video];
  const stream = { getAudioTracks: () => tracks.filter(t => (t as MediaStreamTrack).kind === "audio"), getVideoTracks: () => [video], getTracks: () => tracks, addTrack: (t: unknown) => tracks.push(t), removeTrack: vi.fn() };
  useScreen.setState({ own: { phase: "choosing", attempt: 1, share: null, stream: stream as unknown as MediaStream, surface: "screen", spec: legacySpec("text"), viewers: 0, guests: 0, guestLink: false, preview: false, computerSound } });
  return { stream };
}
const own = () => useScreen.getState().own;

beforeEach(() => {
  sound.instances.length = 0; sound.receivers.length = 0; sound.result = liveTrack(); sound.receivable = true;
  media.publishers.length = 0; media.transports.length = 0;
  useSettings.setState({ shareComputerSound: true, shareAudio: {} });
  useUi.setState({ toast: vi.fn() });
  useScreen.setState({ ...initialScreen });
  useSession.getState().dispatch({ type: "reset" });
  useSession.getState().dispatch({ type: "welcome", resync: false, now: 1, welcome: welcome({ session: 1, uid: "u1", channels: [ch(1)], clients: [cl(1, 1)] }) });
  vi.stubGlobal("RTCPeerConnection", class {});
});
afterEach(() => { vi.unstubAllGlobals(); });

const nativeLink = () => Promise.resolve({ api: {}, owner: 1 }) as never;
const controllerOn = (conn: Link) => new ScreenController(() => conn, String, undefined, undefined, nativeLink);

describe("computer sound", () => {
  it("publishes as native audio from the sharer's confirmation", async () => {
    const screen = controllerOn(link());
    const { stream } = choosing("ok");
    await screen.confirmShare();
    expect(sound.instances).toHaveLength(1);
    expect(media.publishers[0]!.params).toMatchObject({ audio: sound.result, nativeAudio: true });
    expect(stream.getTracks()).toContain(sound.result);
    expect(own()).toMatchObject({ audio: true, computerSound: "ok" });
  });

  it.each([[false, "ok"], [true, "permission"], [true, undefined]] as const)("never starts without a yes the system can honour: wanted %s, system %s", async (wanted, system) => {
    useSettings.setState({ shareComputerSound: wanted });
    const screen = controllerOn(link());
    choosing(system);
    await screen.confirmShare();
    expect(sound.instances).toHaveLength(0);
    expect(media.publishers[0]!.params.audio).toBeUndefined();
    expect(media.publishers[0]!.params.nativeAudio).toBe(false);
  });

  it.each(["permission", "unsupported", "failed"] as const)("a %s refusal leaves the picture running and says why", async state => {
    sound.result = state;
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    expect(media.publishers).toHaveLength(1);
    expect(media.publishers[0]!.params.audio).toBeUndefined();
    expect(own()).toMatchObject({ phase: "starting", audio: false, computerSound: state });
    expect(sound.instances[0]!.stop).toHaveBeenCalled();
  });

  it("stops capturing for a server without share sound", async () => {
    const screen = controllerOn(link(false));
    choosing("ok");
    await screen.confirmShare();
    expect(sound.instances[0]!.stop).toHaveBeenCalled();
    expect(sound.instances[0]!.start).not.toHaveBeenCalled();
    expect(own()).toMatchObject({ audio: false, audioServer: false });
  });

  it("keeps the same capture across a publisher recovery, with no second question", async () => {
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    const first = media.publishers[0]!;
    useScreen.getState().dispatch({ type: "recoverOwn" });
    (first.params as unknown as { state(s: string): void }).state("failed");
    await screen.confirmShare();
    expect(sound.instances).toHaveLength(1);
    expect(sound.instances[0]!.stop).not.toHaveBeenCalled();
    const replacement = media.publishers.at(-1)!;
    expect(replacement.params).toMatchObject({ audio: sound.result, nativeAudio: true });
  });

  it("takes consent back with one action, the picture going on", async () => {
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    screen.stopComputerSound();
    expect(sound.instances[0]!.stop).toHaveBeenCalled();
    expect(own()).toMatchObject({ audio: false, computerSound: "stopped" });
    expect(own()).not.toBeNull();
  });

  it("says when sound is coming back, and what stopped it from coming back", async () => {
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    sound.instances[0]!.resuming(true);
    expect(own()).toMatchObject({ computerSound: "resuming", audio: true });
    sound.instances[0]!.resuming(false);
    expect(own()).toMatchObject({ computerSound: "ok", audio: true });
    sound.instances[0]!.resuming(true);
    sound.instances[0]!.ended("permission");
    expect(own()).toMatchObject({ computerSound: "permission", audio: false });
  });

  it("lets the sharer take sound back while it is coming back", async () => {
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    sound.instances[0]!.resuming(true);
    screen.stopComputerSound();
    expect(sound.instances[0]!.stop).toHaveBeenCalled();
    expect(own()).toMatchObject({ computerSound: "stopped", audio: false });
  });

  it("reports a capture the OS ended, and ends with the share", async () => {
    const screen = controllerOn(link());
    choosing("ok");
    await screen.confirmShare();
    sound.instances[0]!.ended("failed");
    expect(own()).toMatchObject({ computerSound: "failed", audio: false });
    choosing("ok");
    await screen.confirmShare();
    await screen.stopShare();
    expect(sound.instances[1]!.stop).toHaveBeenCalled();
  });
});

describe("watching on the desktop app", () => {
  const start = async (screen: ScreenController, conn: Link) => { await (screen as unknown as { ensureTransport(c: Link): unknown; }).ensureTransport(conn); };
  const negotiated = async (receivable: boolean, server = true) => {
    sound.receivable = receivable;
    const conn = link(server);
    const screen = new ScreenController(() => conn, String, undefined, undefined, nativeLink);
    void start(screen, conn).catch(() => {});
    return { screen, enabled: await media.transports[0]!.params.audio!() };
  };

  it("negotiates sound with a receiver the webview cannot decode for", async () => {
    const { enabled } = await negotiated(true);
    expect(enabled).toBe(true);
    expect(sound.receivers).toHaveLength(1);
  });

  it.each([[false, true], [true, false]])("stays video only when the webview could play it or the server has none: %s %s", async (receivable, server) => {
    const { enabled } = await negotiated(receivable, server);
    expect(enabled).toBe(false);
    expect(sound.receivers).toHaveLength(0);
  });

  it("opens a slot at its saved volume and takes mute as zero", async () => {
    useSettings.setState({ shareAudio: { A: { volume: 0.4, muted: false }, B: { volume: 0.9, muted: true } } });
    const { screen } = await negotiated(true);
    useScreen.setState({ watches: [{ share: "A", id: 1, slot: 2, status: "watching" }] });
    await screen.openSound(1, "A", 2);
    expect(sound.receivers[0]!.slots).toEqual([[2, 0.4]]);
    expect(useScreen.getState().watches[0]!.soundReady).toBe(true);
    screen.setSoundGain("B", 2);
    expect(sound.receivers[0]!.gains).toEqual([[2, 0]]);
  });
});
