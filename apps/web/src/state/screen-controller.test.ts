import { legacySpec, type ScreenSpec } from "../voice/screen-quality";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RequestError, type Link } from "../net/connection";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { ScreenController } from "./screen-controller";
import { initialScreen, useScreen } from "./screen";
import { useSession, useUi } from "./stores";
import { useSettings } from "./settings";
import { useUplink } from "./uplink";

interface PublisherOptions {
  uplink?(status: import("../voice/uplink-congestion").UplinkStatus | null): void;
  ended(): void;
  fallbacks?: import("../voice/screen").EncoderFallbacks;
  quality: ScreenSpec; legacy: boolean; automaticMode: boolean;
  surfaceChanged(surface: "screen" | "window" | "tab" | "unknown", spec: ScreenSpec): void;
  offer(body: object): Promise<string>; state(s: string): void; encodeFailed(): void;
}

const media = vi.hoisted(() => ({
  transports: [] as Array<{ params: { run: number; state(s: string): void; offer(sdp: string, run: number): Promise<string> }; close: () => void }>,
  publishers: [] as Array<{ params: PublisherOptions; fallbacks: () => import("../voice/screen").EncoderFallbacks; close: () => void; setSpec: (spec: ScreenSpec, automaticMode: boolean) => Promise<void> }>,
}));
vi.mock("../voice/screen", async importOriginal => {
  const actual = await importOriginal<typeof import("../voice/screen")>();
  class ScreenWatchTransport {
    readonly run: number;
    close = vi.fn();
    clearSlot = vi.fn();
    keyframe = vi.fn();
    constructor(readonly params: { run: number; state(s: string): void; offer(sdp: string, run: number): Promise<string> }) { this.run = params.run; media.transports.push(this); }
    async start() { await this.params.offer("watch-offer", this.run); }
    async stats() { return null; }
  }
  class ScreenPublisher {
    fallbacks = vi.fn(() => ({ startHintUnsafe: false, brokenCodecs: [], codecFellBack: false, av1Retired: false, recovery: null }));
    close = vi.fn();
    setSpec = vi.fn(async () => {});
    constructor(readonly params: PublisherOptions) { media.publishers.push(this); }
    async start() { await this.params.offer({ ssrc: 1, rtx_ssrc: 2, sdp: "publish-offer" }); }
  }
  return { ...actual, ScreenWatchTransport, ScreenPublisher, applyCaptureSpec: vi.fn(async () => {}), captureSupport: () => "ok" as const };
});

const ch = (id: number, encrypted = false): Channel => ({ id, parent: null, name: `c${id}`, topic: "", position: id, has_password: false, max_clients: null, encrypted, history: false, private: false, synced: false });
const cl = (id: number, channel: number | null, extra: Partial<Client> = {}): Client => ({
  id, uid: `u${id}`, nickname: `n${id}`, channel, groups: [2], platform: "web", muted: false, deafened: false, away: null, talking: false, voice: false,
  voice_binding: channel === null ? null : { call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", run: 1 }, ...extra,
});
const share = (id: string, client: number, channel: number) => ({ share: id, client, channel, preset: "text" as const, surface: "tab" as const });

type Handler = (d: Record<string, unknown>) => unknown;
function fakeLink(handlers: Record<string, Handler>) {
  const calls: Array<[string, Record<string, unknown>]> = [];
  const link = {
    request: vi.fn(async (op: string, d: Record<string, unknown>) => {
      if (op === "screen.spec.capabilities") return handlers[op]?.(d) ?? { screen_spec: false };
      if (op === "screen.capabilities") return handlers[op]?.(d) ?? { screen_h264: false };
      calls.push([op, d]);
      const handler = handlers[op];
      if (!handler) return {};
      return handler(d);
    }),
  } as unknown as Link;
  return { link, calls, ops: () => calls.map(([op]) => op) };
}

const toast = vi.fn();
function deferred<T>() {
  let resolve!: (v: T) => void, reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}
const flush = () => vi.advanceTimersByTimeAsync(0);

const subscriptions: Array<() => void> = [];
function initialise(screen: ScreenController): void {
  const subscribe = useSession.subscribe;
  const spy = vi.spyOn(useSession, "subscribe").mockImplementation(listener => {
    const off = subscribe(listener); subscriptions.push(off); return off;
  });
  screen.init(); spy.mockRestore();
}

beforeEach(() => {
  vi.useFakeTimers();
  media.transports.length = 0;
  media.publishers.length = 0;
  useSettings.setState({ screenMode: null, screenHeight: 1080, screenFps: 30 });
  toast.mockReset();
  useUi.setState({ toast });
  useScreen.setState({ ...initialScreen });
  useSession.getState().dispatch({ type: "reset" });
  useSession.getState().dispatch({
    type: "welcome", resync: false, now: 1,
    welcome: welcome({ session: 1, uid: "u1", channels: [ch(1), ch(2), ch(3, true)], clients: [cl(1, null), cl(2, 1, { sharing: true }), cl(3, 3, { sharing: true })], shares: [share("A", 2, 1), share("E", 3, 3)] }),
  });
  vi.stubGlobal("RTCPeerConnection", class {});
});
afterEach(() => { for (const off of subscriptions.splice(0)) off(); vi.useRealTimers(); vi.unstubAllGlobals(); });

describe("watching", () => {
  it("retries rate-limited watch offers after refill with a fresh run", async () => {
    let offers = 0;
    const { link } = fakeLink({
      "screen.watch.offer": () => { if (offers++ === 0) throw new RequestError("rate_limited", "too often"); return { sdp: "answer" }; },
      "screen.watch": () => ({ slot: 0 }),
    });
    const screen = new ScreenController(() => link, String);
    const watching = screen.watch("A");
    await flush();
    expect(media.transports[0]!.close).toHaveBeenCalled();
    expect(toast).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(15_250);
    expect(media.transports).toHaveLength(2);
    expect(media.transports[1]!.params.run).toBeGreaterThan(media.transports[0]!.params.run);
    media.transports[1]!.params.state("connected");
    await watching;
    expect(useScreen.getState().watches[0]?.status).toBe("watching");
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it("ignores a stale error from a superseded watch run", async () => {
    const answer = deferred<{ sdp: string }>();
    let offers = 0;
    const { link } = fakeLink({
      "screen.watch.offer": () => offers++ === 0 ? answer.promise : { sdp: "answer" },
      "screen.watch": () => ({ slot: 0 }),
    });
    const screen = new ScreenController(() => link, String);
    const first = screen.watch("A");
    await flush();
    screen.reset();
    await first;
    const fresh = screen.watch("A");
    await flush();
    answer.reject(new RequestError("stale", "run superseded"));
    await flush();
    media.transports[1]!.params.state("connected");
    await fresh;
    expect(useScreen.getState().watches[0]?.status).toBe("watching");
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it("watches without joining voice: one watch transport, then the slot, then focus", async () => {
    let unavailable = 2;
    const { link, ops, calls } = fakeLink({
      "screen.watch.offer": () => ({ sdp: "answer" }),
      "screen.watch": d => { if (unavailable-- > 0) throw new RequestError("unavailable", "connect a watch transport first"); return { slot: d.share === "A" ? 1 : 2 }; },
    });
    const screen = new ScreenController(() => link, String);
    const watching = screen.watch("A");
    expect(useScreen.getState().stage).toBe("A");
    await flush();
    const transport = media.transports[0]!;
    transport.params.state("connected");
    await vi.advanceTimersByTimeAsync(1000);
    await watching;
    expect(ops()).toEqual(["screen.watch.offer", "screen.watch", "screen.watch", "screen.watch", "screen.focus"]);
    expect(calls[0]![1]).toMatchObject({ run: transport.params.run, sdp: "watch-offer" });
    expect(useScreen.getState().watches).toEqual([{ share: "A", id: expect.any(Number), slot: 1, status: "watching" }]);
    // A second watch reuses the transport.
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "screen.started", d: share("B", 2, 2) } });
    await screen.watch("B").catch(() => {});
    expect(media.transports).toHaveLength(1);
  });

  it("never watches in an encrypted channel before encrypted shares exist", async () => {
    const { link, ops } = fakeLink({});
    await new ScreenController(() => link, String).watch("E");
    expect(ops()).toEqual([]);
    expect(useScreen.getState().watches).toEqual([]);
    expect(toast).toHaveBeenCalledWith("info", "Screen sharing in encrypted channels is coming soon.");
  });

  it("says when every slot is taken", async () => {
    const { link } = fakeLink({ "screen.watch.offer": () => ({ sdp: "a" }), "screen.watch": () => { throw new RequestError("busy", "you are watching as many shares as allowed"); } });
    useScreen.setState({ watches: ["W", "X", "Y", "Z"].map((share, slot) => ({ share, id: 100 + slot, slot, status: "watching" as const })) });
    const screen = new ScreenController(() => link, String);
    const watching = screen.watch("A");
    await flush();
    media.transports[0]!.params.state("connected");
    await watching;
    expect(toast).toHaveBeenCalledWith("error", "You can watch up to 4 screen shares at once. Stop watching one first.");
    expect(useScreen.getState().watches.map(w => w.share)).toEqual(["W", "X", "Y", "Z"]);
  });

  it("drops a watch when its share stops, and tells a viewer that lost access", () => {
    const screen = new ScreenController(() => null, String);
    useScreen.setState({ watches: [{ share: "A", id: 100, slot: 0, status: "watching" }], stage: "A" });
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "access" } });
    expect(useScreen.getState().stage).toBeNull();
    expect(toast).toHaveBeenCalledWith("info", "You can no longer watch this screen share.");
  });
});

describe("reused receive slots", () => {
  function setup(holdGrant = false) {
    for (const id of ["B", "C", "D"]) useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "screen.started", d: share(id, 2, 2) } });
    let offers = 0;
    const pending = deferred<{ slot: number }>();
    const grant = deferred<{ slot: number }>();
    const { link, ops } = fakeLink({
      "screen.watch.offer": () => { offers++; return { sdp: "answer" }; },
      "screen.watch": d => offers === 1
        ? d.share === "D" ? pending.promise : d.share === "C" && holdGrant ? grant.promise : { slot: d.share === "A" ? 0 : 1 }
        : { slot: d.share === "C" ? 0 : d.share === "A" ? 1 : 2 },
    });
    const screen = new ScreenController(() => link, String);
    const start = async (otherWatch = true) => {
      const watching = screen.watch(otherWatch ? "A" : "B");
      await flush();
      media.transports[0]!.params.state("connected");
      await watching;
      if (otherWatch) await screen.watch("B");
      return media.transports[0]!;
    };
    return { screen, start, pending, grant, ops };
  }

  it.each(["unwatch", "release"])("renews a reused slot after %s when it is the only watch", async how => {
    const { screen, start } = setup();
    const old = await start(false);
    const stream = {} as MediaStream;
    useScreen.getState().dispatch({ type: "stream", run: old.params.run, slot: 1, stream });
    if (how === "unwatch") await screen.unwatch("B");
    else {
      screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 1, share: null } });
      screen.handleEvent({ ev: "screen.stopped", d: { share: "B", reason: "stopped" } });
      toast.mockReset();
    }
    // Keep the track for this run; only a new run can supply a new receiver.
    expect(useScreen.getState().streams[1]).toBe(stream);
    const watching = screen.watch("C");
    await flush();
    expect(old.close).toHaveBeenCalledOnce();
    expect(media.transports).toHaveLength(2);
    expect(useScreen.getState().watches).toMatchObject([{ share: "C", status: "requesting", slot: null }]);
    expect(useScreen.getState().streams.every(s => s === null)).toBe(true);
    media.transports[1]!.params.state("connected");
    await watching;
    await flush();
    expect(useScreen.getState().watches).toMatchObject([{ share: "C", status: "watching", slot: 0 }]);
    // Events from the retired run cannot free the new watch's slot.
    screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 0, share: null } });
    expect(useScreen.getState().watches).toHaveLength(1);
    expect(useScreen.getState().stage).toBe("C");
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it.each(["unwatch", "release"])("keeps the receiver and other active watches uninterrupted after %s", async how => {
    const { screen, start, ops } = setup();
    const old = await start();
    const active = useScreen.getState().watches.find(w => w.share === "A")!;
    const streams = [{} as MediaStream, {} as MediaStream];
    streams.forEach((stream, slot) => useScreen.getState().dispatch({ type: "stream", run: old.params.run, slot, stream }));
    if (how === "unwatch") await screen.unwatch("B");
    else {
      screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 1, share: null } });
      screen.handleEvent({ ev: "screen.stopped", d: { share: "B", reason: "stopped" } });
      toast.mockReset();
    }
    await screen.watch("C");
    expect(media.transports).toHaveLength(1);
    expect(old.close).not.toHaveBeenCalled();
    expect(useScreen.getState().transport).toEqual({ run: old.params.run, phase: "connected" });
    expect(useScreen.getState().watches.find(w => w.share === "A")).toBe(active);
    expect(useScreen.getState().streams.slice(0, 2)).toEqual(streams);
    expect(useScreen.getState().watches).toMatchObject([
      { share: "A", status: "watching", slot: 0 }, { share: "C", status: "watching", slot: 1 },
    ]);
    expect(ops().filter(op => op === "screen.watch.offer")).toHaveLength(1);
    expect(ops().filter(op => op === "screen.watch")).toHaveLength(3);
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it("does not renew while another watch is pending", async () => {
    const { screen, start, pending } = setup();
    const old = await start(false);
    await screen.unwatch("B");
    const slow = screen.watch("D");
    await flush();
    await screen.watch("C");
    expect(media.transports).toHaveLength(1);
    expect(old.close).not.toHaveBeenCalled();
    pending.resolve({ slot: 2 });
    await slow;
    expect(useScreen.getState().watches).toMatchObject([
      { share: "D", status: "watching", slot: 2 }, { share: "C", status: "watching", slot: 1 },
    ]);
    screen.reset();
  });

  it("checks for other watches when the reused-slot grant arrives", async () => {
    const { screen, start, grant } = setup(true);
    const old = await start(false);
    await screen.unwatch("B");
    const watching = screen.watch("C");
    await flush();
    await screen.watch("A");
    grant.resolve({ slot: 1 });
    await watching;
    expect(media.transports).toHaveLength(1);
    expect(old.close).not.toHaveBeenCalled();
    expect(useScreen.getState().watches).toMatchObject([
      { share: "C", status: "watching", slot: 1 }, { share: "A", status: "watching", slot: 0 },
    ]);
    screen.reset();
  });

  it("keeps waiting watches on stage through a transport failure", async () => {
    const { screen, start } = setup();
    await start(false);
    await screen.unwatch("B");
    const watching = screen.watch("C");
    await flush();
    media.transports[1]!.params.state("failed");
    await watching;
    await flush();
    expect(useScreen.getState().watches).toMatchObject([{ share: "C", status: "requesting" }]);
    expect(useScreen.getState().stage).toBe("C");
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it.each([false, true])("bounds a silent receiver with another active watch = %s, fencing old timeouts", async otherWatch => {
    const { screen, start, ops } = setup();
    await start(otherWatch);
    await screen.unwatch("B");
    const watching = screen.watch("C");
    await flush();
    if (!otherWatch) media.transports[1]!.params.state("connected");
    await watching;
    const { transport, watches } = useScreen.getState();
    const watch = watches.find(w => w.share === "C")!;
    screen.frameFailed("C", transport.run - 1, watch.id);
    screen.frameFailed("C", transport.run, watch.id - 1);
    expect(useScreen.getState().watches).toHaveLength(otherWatch ? 2 : 1);
    expect(toast).not.toHaveBeenCalled();
    screen.frameFailed("C", transport.run, watch.id);
    await flush();
    expect(useScreen.getState().watches.map(w => w.share)).toEqual(otherWatch ? ["A"] : []);
    expect(ops().filter(op => op === "screen.unwatch")).toHaveLength(2);
    expect(toast).toHaveBeenCalledExactlyOnceWith("error", "The connection for watching screen shares was lost.");
    screen.reset();
  });

  it("clears only the matching first-frame deadline and never rearms it", async () => {
    const { screen, start } = setup();
    const old = await start(false);
    const watch = useScreen.getState().watches[0]!;
    const failed = vi.spyOn(screen, "frameFailed");
    expect(vi.getTimerCount()).toBe(1);
    screen.frameReceived("B", old.params.run - 1, watch.id);
    screen.frameReceived("B", old.params.run, watch.id - 1);
    screen.frameReceived("A", old.params.run, watch.id);
    expect(vi.getTimerCount()).toBe(1);
    screen.frameReceived("B", old.params.run, watch.id);
    expect(vi.getTimerCount()).toBe(0);
    screen.frameReceived("B", old.params.run, watch.id);
    await vi.advanceTimersByTimeAsync(70_000);
    expect(failed).not.toHaveBeenCalled();
    expect(useScreen.getState().watches).toHaveLength(1);
    screen.reset();
  });

  it("fences player repair by run/watch ID without restarting the first-frame deadline", async () => {
    const { screen, start, ops } = setup();
    const old = await start(false);
    const watch = useScreen.getState().watches[0]!;
    screen.requestFrame("B", old.params.run - 1, watch.id);
    screen.requestFrame("B", old.params.run, watch.id - 1);
    screen.requestFrame("A", old.params.run, watch.id);
    expect(ops()).not.toContain("screen.keyframe");
    screen.requestFrame("B", old.params.run, watch.id);
    expect(ops().filter(op => op === "screen.keyframe")).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(1);
    await screen.unwatch("B");
    screen.requestFrame("B", old.params.run, watch.id);
    expect(ops().filter(op => op === "screen.keyframe")).toHaveLength(1);
    screen.reset();
  });

  it.each(["unwatch", "release", "stop", "failure", "reset"])("clears a pending first-frame deadline on %s", async how => {
    const { screen, start } = setup();
    const old = await start(false);
    const failed = vi.spyOn(screen, "frameFailed");
    expect(vi.getTimerCount()).toBe(1);
    if (how === "unwatch") await screen.unwatch("B");
    if (how === "release") screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 1, share: null } });
    if (how === "stop") screen.handleEvent({ ev: "screen.stopped", d: { share: "B", reason: "stopped" } });
    if (how === "failure") old.params.state("failed");
    if (how === "reset") screen.reset();
    expect(vi.getTimerCount()).toBe(how === "failure" ? 2 : 0);
    if (how === "failure") screen.reset();
    await vi.advanceTimersByTimeAsync(35_000);
    expect(failed).not.toHaveBeenCalled();
    screen.reset();
  });

  it("holds the first-frame deadline while the server pauses the share and starts it over on resume", async () => {
    const { screen, start } = setup();
    const old = await start(false);
    const watch = useScreen.getState().watches[0]!;
    const failed = vi.spyOn(screen, "frameFailed");
    expect(vi.getTimerCount()).toBe(1);
    await vi.advanceTimersByTimeAsync(20_000);
    // Paused before its first frame: however long the pause, the wait does not run out.
    screen.handleEvent({ ev: "screen.paused", d: { share: "B", reason: "bandwidth" } });
    expect(vi.getTimerCount()).toBe(0);
    await vi.advanceTimersByTimeAsync(40_000);
    expect(failed).not.toHaveBeenCalled();
    expect(useScreen.getState().watches).toMatchObject([{ share: "B", status: "watching", paused: "bandwidth" }]);
    // Resumed: a full 35 s wait again, not what was left of the first one.
    screen.handleEvent({ ev: "screen.resumed", d: { share: "B" } });
    expect(vi.getTimerCount()).toBe(1);
    await vi.advanceTimersByTimeAsync(34_999);
    expect(failed).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(failed).toHaveBeenCalledExactlyOnceWith("B", old.params.run, watch.id);
    screen.reset();
  });
});

describe("watch requests (review round 1)", () => {
  it("a retry of a watch whose transport failed never reaches the replacement transport", async () => {
    const { link, calls } = fakeLink({
      "screen.watch.offer": () => ({ sdp: "answer" }),
      "screen.watch": d => { if (d.share === "A") throw new RequestError("unavailable", "connect a watch transport first"); return { slot: 0 }; },
    });
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "screen.started", d: share("B", 2, 2) } });
    const screen = new ScreenController(() => link, String);
    const watchingA = screen.watch("A");
    await flush();
    media.transports[0]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(300);
    media.transports[0]!.params.state("failed");
    const stoppingA = screen.unwatch("A");
    await vi.advanceTimersByTimeAsync(250);
    await stoppingA;
    const watchingB = screen.watch("B");
    await flush();
    media.transports[1]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(6_000);
    await Promise.all([watchingA, watchingB]);
    const asksForA = calls.filter(([op, d]) => op === "screen.watch" && d.share === "A").length;
    await vi.advanceTimersByTimeAsync(6_000);
    expect(calls.filter(([op, d]) => op === "screen.watch" && d.share === "A").length).toBe(asksForA);
    expect(useScreen.getState().watches).toEqual([{ share: "B", id: expect.any(Number), slot: 0, status: "watching" }]);
    expect(toast).not.toHaveBeenCalled();
  });

  it("a slot granted to a failed run cannot resurrect its retired grant", async () => {
    const answer = deferred<{ slot: number }>();
    const { link, ops } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => answer.promise });
    const screen = new ScreenController(() => link, String);
    const watching = screen.watch("A");
    await flush();
    media.transports[0]!.params.state("connected");
    await flush();
    media.transports[0]!.params.state("failed");
    answer.resolve({ slot: 0 });
    await watching;
    expect(ops()).toEqual(["screen.watch.offer", "screen.watch"]);
    expect(useScreen.getState().watches).toMatchObject([{ share: "A", status: "requesting", slot: null }]);
    screen.reset();
  });

  it("watch → stop → watch keeps the second watch, whatever order the first one's answers arrive in", async () => {
    const answers = [deferred<{ slot: number }>(), deferred<{ slot: number }>()];
    let asked = 0;
    const { link, ops } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => answers[asked++]!.promise });
    const screen = new ScreenController(() => link, String);
    const first = screen.watch("A");
    await flush();
    const transport = media.transports[0]!;
    transport.params.state("connected");
    await flush();
    const run = transport.params.run;
    const stopping = screen.unwatch("A");
    const second = screen.watch("A");
    // The server assigns, answers and then releases the first watch's slot.
    screen.handleEvent({ ev: "screen.slot", d: { run, slot: 0, share: "A" } });
    answers[0]!.resolve({ slot: 0 });
    await flush();
    screen.handleEvent({ ev: "screen.slot", d: { run, slot: 0, share: null } });
    await flush();
    screen.handleEvent({ ev: "screen.slot", d: { run, slot: 1, share: "A" } });
    answers[1]!.resolve({ slot: 1 });
    await Promise.all([first, stopping, second]);
    expect(useScreen.getState().watches).toEqual([{ share: "A", id: expect.any(Number), slot: 1, status: "watching" }]);
    // The second request goes out only after the first watch's release.
    expect(ops().slice(1)).toEqual(["screen.watch", "screen.unwatch", "screen.unwatch", "screen.watch", "screen.focus"]);
  });

  it("says why a watched share ended when its slot was freed first, as the server sends it", () => {
    const screen = new ScreenController(() => null, String);
    useScreen.setState({ transport: { run: 7, phase: "connected" }, watches: [{ share: "A", id: 1, slot: 0, status: "watching" }], stage: "A" });
    screen.handleEvent({ ev: "screen.slot", d: { run: 7, slot: 0, share: null } });
    expect(useScreen.getState().watches).toMatchObject([{ share: "A", status: "requesting" }]);
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "access" } });
    expect(useScreen.getState().watches).toEqual([]);
    expect(toast).toHaveBeenCalledWith("info", "You can no longer watch this screen share.");
    toast.mockReset();
    useScreen.setState({ watches: [{ share: "B", id: 2, slot: 1, status: "watching" }], stage: "B" });
    screen.handleEvent({ ev: "screen.slot", d: { run: 7, slot: 1, share: null } });
    screen.handleEvent({ ev: "screen.stopped", d: { share: "B", reason: "stopped" } });
    expect(toast).toHaveBeenCalledWith("info", "The screen share has ended.");
    // A share I never watched ends quietly.
    toast.mockReset();
    screen.handleEvent({ ev: "screen.stopped", d: { share: "C", reason: "access" } });
    expect(toast).not.toHaveBeenCalled();
  });

  it("a failed watch transport says so once, from either failure path", async () => {
    for (const how of ["state", "start"] as const) {
      toast.mockReset();
      useScreen.setState({ ...initialScreen });
      const { link } = fakeLink({
        "screen.watch.offer": () => { if (how === "start") throw new Error("offer refused"); return { sdp: "answer" }; },
        "screen.watch": () => ({ slot: 0 }),
      });
      const screen = new ScreenController(() => link, String);
      const watching = screen.watch("A");
      await flush();
      if (how === "state") {
        media.transports.at(-1)!.params.state("connected");
        await watching;
        expect(useScreen.getState().watches).toHaveLength(1);
        media.transports.at(-1)!.params.state("failed");
      }
      await watching;
      for (const delay of [1_000, 2_000, 4_000]) {
        expect(toast).not.toHaveBeenCalled();
        await vi.advanceTimersByTimeAsync(delay);
        if (how === "state") media.transports.at(-1)!.params.state("failed");
        await flush();
      }
      expect(useScreen.getState()).toMatchObject({ watches: [], stage: null });
      expect(toast).toHaveBeenCalledTimes(1);
      expect(toast).toHaveBeenCalledWith("error", "The connection for watching screen shares was lost.");
      screen.reset();
    }
  });
});

describe("watch recovery", () => {
  it.each([true, false])("uses the watched channel's password override outside voice (allowed=%s)", async allowed => {
    useSession.setState({ permissions: allowed ? [] : ["bypass_password"], channels: { ...useSession.getState().channels,
      1: { ...ch(1), has_password: true, overrides: [{ user: "u1", allow: allowed ? ["bypass_password"] : [], deny: allowed ? [] : ["bypass_password"] }] } } });
    const { link, calls } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    const watching = screen.watch("A"); await flush(); media.transports[0]!.params.state("connected"); await watching;
    screen.suspend(); screen.resume(); await flush();
    if (allowed) media.transports[1]!.params.state("connected");
    await flush();
    expect(calls.filter(([op]) => op === "screen.watch")).toHaveLength(allowed ? 2 : 1);
    if (!allowed) {
      useSession.setState({ channels: { ...useSession.getState().channels, 1: { ...ch(1), has_password: true, overrides: [{ user: "u1", allow: ["bypass_password"], deny: [] }] } } });
      await flush(); media.transports[1]!.params.state("connected"); await flush();
    }
    expect(useScreen.getState().watches).toMatchObject([{ share: "A", status: "watching" }]);
    expect(toast).not.toHaveBeenCalled(); screen.reset();
  });
  it.each(["session", "sharer"])("does not replace an interrupted capture with the same account's pre-existing share (%s)", async kind => {
    useSession.setState({ clients: { ...useSession.getState().clients, 4: cl(4, 1, { uid: "u2", sharing: true }) }, shares: { ...useSession.getState().shares, B: share("B", 4, 1) } });
    const { link, calls } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    const watching = screen.watch("A"); await flush(); media.transports[0]!.params.state("connected"); await watching;
    if (kind === "session") screen.suspend();
    else screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "left" } });
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "screen.stopped", d: { share: "A", reason: "left" } } });
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.left", d: { client: 2, reason: { kind: "timeout" } } } });
    if (kind === "session") screen.resume();
    await flush();
    expect(useScreen.getState().stage).toBe("A");
    expect(calls.filter(([op]) => op === "screen.watch").map(([, d]) => d.share)).toEqual(["A"]);
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.joined", d: cl(22, 1, { uid: "u2", sharing: true }) } });
    useSession.getState().dispatch({ type: "event", now: 5, event: { ev: "screen.started", d: share("returned", 22, 1) } });
    await flush(); media.transports[1]!.params.state("connected"); await flush();
    expect(useScreen.getState().watches).toMatchObject([{ share: "returned", status: "watching" }]);
    expect(toast).not.toHaveBeenCalled(); screen.reset();
  });
  it("resumes all watches after a transport drop, preserving focus, and ignores stale callbacks", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "screen.started", d: share("B", 2, 2) } });
    const { link, calls } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": d => ({ slot: d.share === "A" ? 0 : 1 }) });
    const screen = new ScreenController(() => link, String);
    const watching = screen.watch("A"); await flush();
    const old = media.transports[0]!;
    old.params.state("connected"); await watching;
    await screen.watch("B"); screen.focus("A");
    old.params.state("failed");
    expect(useScreen.getState().stage).toBe("A");
    expect(toast).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1_000);
    media.transports[1]!.params.state("connected"); await flush();
    old.params.state("failed"); await flush();
    expect(useScreen.getState().watches.map(w => w.status)).toEqual(["watching", "watching"]);
    expect(useScreen.getState().stage).toBe("A");
    expect(calls.filter(([op]) => op === "screen.watch")).toHaveLength(4);
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it.each([null, 2])("ends a voluntary sharer leave/move on the next client update (%s)", async channel => {
    const { link } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    const watching = screen.watch("A"); await flush();
    media.transports[0]!.params.state("connected"); await watching;
    screen.handleEvent({ ev: "screen.slot", d: { run: useScreen.getState().transport.run, slot: 0, share: null } });
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "left" } });
    expect(useScreen.getState().stage).toBe("A");
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(2, channel, { sharing: false }) } });
    await vi.advanceTimersByTimeAsync(150);
    expect(useScreen.getState().stage).toBeNull();
    expect(toast).toHaveBeenCalledExactlyOnceWith("info", "The screen share has ended.");
    await vi.advanceTimersByTimeAsync(60_000);
    expect(toast).toHaveBeenCalledTimes(1);
    screen.reset();
  });

  it("reports an ended share when its return window expires on a healthy viewer transport", async () => {
    const { link } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    const watching = screen.watch("A"); await flush();
    media.transports[0]!.params.state("connected"); await watching;
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "left" } });
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.left", d: { client: 2, reason: { kind: "timeout" } } } });
    await vi.advanceTimersByTimeAsync(45_000);
    expect(useScreen.getState().stage).toBeNull();
    expect(toast).toHaveBeenCalledExactlyOnceWith("info", "The screen share has ended.");
    screen.reset();
  });

  it("queues a watch selected during session reconnect and lets Stop cancel it", async () => {
    const { link, calls } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String);
    screen.suspend(); await screen.watch("A");
    expect(calls).toEqual([]);
    expect(useScreen.getState().stage).toBe("A");
    screen.resume(); await flush();
    media.transports[0]!.params.state("connected"); await flush();
    expect(useScreen.getState().watches).toMatchObject([{ share: "A", status: "watching" }]);
    screen.suspend(); await screen.unwatch("A"); screen.resume(); await flush();
    expect(useScreen.getState().stage).toBeNull();
    expect(media.transports).toHaveLength(1);
    screen.reset();
  });

  it.each(["session", "sharer"])("rewatches a returning sharer's new ID after %s reconnect", async kind => {
    const { link } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    const watching = screen.watch("A"); await flush();
    media.transports[0]!.params.state("connected"); await watching;
    if (kind === "session") screen.suspend();
    else {
      screen.handleEvent({ ev: "screen.slot", d: { run: useScreen.getState().transport.run, slot: 0, share: null } });
      screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "left" } });
    }
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "screen.stopped", d: { share: "A", reason: "left" } } });
    expect(useScreen.getState().stage).toBe("A");
    // Core.remove first broadcasts its move-to-null client update, then client.left.
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(2, null, { sharing: false }) } });
    await vi.advanceTimersByTimeAsync(40);
    expect(useScreen.getState().stage).toBe("A");
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.left", d: { client: 2, reason: { kind: "timeout" } } } });
    const returned = share("new", 22, 1);
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.joined", d: cl(22, 1, { uid: "u2", sharing: true }) } });
    useSession.getState().dispatch({ type: "event", now: 5, event: { ev: "screen.started", d: returned } });
    if (kind === "session") screen.resume();
    screen.handleEvent({ ev: "screen.started", d: returned }); await flush();
    media.transports[1]!.params.state("connected");
    await flush();
    expect(useScreen.getState().stage).toBe("new");
    expect(useScreen.getState().watches).toMatchObject([{ share: "new", status: "watching" }]);
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });
});

describe("watch requests across a reset (review round 2)", () => {
  it.each([false, true])("gates watch extensions on the server advertisement (%s)", async screenH264 => {
    const { link, calls } = fakeLink({ "screen.capabilities": () => {
      if (!screenH264) throw new RequestError("bad_request", "malformed request");
      return { screen_h264: true };
    }, "screen.watch.offer": body => {
      expect(Object.keys(body).sort()).toEqual(screenH264 ? ["browser", "h264", "run", "sdp"] : ["run", "sdp"]);
      return { sdp: "answer" };
    } });
    const screen = new ScreenController(() => link, String);
    const pending = screen.watch("A");
    await flush();
    expect(calls).toHaveLength(1);
    screen.reset();
    await pending;
  });
  it("sends no offer after reset while the feature query is pending", async () => {
    const features = deferred<{ screen_h264: boolean }>();
    const { link, calls } = fakeLink({ "screen.capabilities": () => features.promise });
    const screen = new ScreenController(() => link, String);
    const pending = screen.watch("A");
    await flush();
    screen.reset();
    features.resolve({ screen_h264: true });
    await flush();
    await pending;
    expect(calls).toEqual([]);
  });
  it("a reset before the watch transport connects never blocks watching the same share again", async () => {
    const { link, calls } = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 0 }) });
    const screen = new ScreenController(() => link, String);
    const first = screen.watch("A");
    await flush();
    expect(calls.map(([op]) => op)).toEqual(["screen.watch.offer"]);
    expect(calls[0]![1]).toEqual({ run: expect.any(Number), sdp: "watch-offer" });
    // Answered, not connected yet; then the session resets (as on a reconnect).
    screen.reset();
    await first;
    const again = screen.watch("A");
    await flush();
    media.transports[1]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(100);
    await again;
    expect(calls.map(([op]) => op)).toEqual(["screen.watch.offer", "screen.watch.offer", "screen.watch", "screen.focus"]);
    expect(useScreen.getState().watches).toEqual([{ share: "A", id: expect.any(Number), slot: 0, status: "watching" }]);
  });

  it("requests of the old session send nothing on the new one, and do not hold it up", async () => {
    const old = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => { throw new RequestError("unavailable", "connect a watch transport first"); } });
    const fresh = fakeLink({ "screen.watch.offer": () => ({ sdp: "answer" }), "screen.watch": () => ({ slot: 2 }) });
    let current = old.link;
    const screen = new ScreenController(() => current, String);
    const first = screen.watch("A");
    await flush();
    media.transports[0]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(100);
    const stopping = screen.unwatch("A");
    // The session is replaced while the first watch is still retrying and its release is queued.
    screen.reset();
    current = fresh.link;
    const again = screen.watch("A");
    await flush();
    media.transports[1]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(6_000);
    await Promise.all([first, stopping, again]);
    expect(old.ops().filter(op => op === "screen.watch").length).toBeLessThanOrEqual(1);
    expect(old.ops()).not.toContain("screen.unwatch");
    expect(fresh.ops()).toEqual(["screen.watch.offer", "screen.watch", "screen.focus"]);
    expect(useScreen.getState().watches).toEqual([{ share: "A", id: expect.any(Number), slot: 2, status: "watching" }]);
  });
});

describe("sharing", () => {
  const track = () => ({ readyState: "live", stop: vi.fn(), addEventListener: vi.fn(), getSettings: () => ({ displaySurface: "browser" }) });
  const choosing = (t = track()) => {
    useScreen.setState({ own: { phase: "choosing", attempt: 1, share: null, stream: { getVideoTracks: () => [t], getTracks: () => [t] } as unknown as MediaStream, surface: "tab", spec: legacySpec("text"), viewers: 0, guests: 0, guestLink: true, preview: false } });
    return t;
  };

  it("retains capture when media fails 15 seconds before signalling detects the outage", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    let starts = 0;
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: `M${++starts}` }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen); const source = choosing();
    await screen.confirmShare(); screen.handleEvent({ ev: "screen.started", d: share("M1", 1, 1) });
    const stream = useScreen.getState().own!.stream;
    const watched = useScreen.getState().watches, transport = useScreen.getState().transport;
    const old = media.publishers[0]!; old.params.state("failed");
    expect(useScreen.getState()).toMatchObject({ own: { phase: "reconnecting", stream }, watches: watched, transport });
    await vi.advanceTimersByTimeAsync(15_000);
    expect(useSession.getState().phase).toBe("online");
    expect(source.stop).not.toHaveBeenCalled(); expect(useScreen.getState().notice).toBeNull();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    screen.suspend();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
    screen.resume();
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, 1) } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(2);
    expect(useScreen.getState().own?.stream).toBe(stream); expect(source.stop).not.toHaveBeenCalled();
    screen.handleEvent({ ev: "screen.started", d: share("M2", 1, 1) });
    old.params.state("failed"); await vi.advanceTimersByTimeAsync(45_000);
    expect(useScreen.getState().own?.phase).toBe("live"); expect(toast).not.toHaveBeenCalled(); screen.reset();
  });

  it.each(["transport", "moderator"] as const)("handles the server retiring failed media for %s", async reason => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen); const source = choosing();
    await screen.confirmShare(); screen.handleEvent({ ev: "screen.link", d: { share: "M", active: true } });
    media.publishers[0]!.params.state("failed");
    expect(useScreen.getState().own?.guestLink).toBe(true);
    await screen.confirmShare(); expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    const revoked = screen.turnOffLink("M"); expect(useScreen.getState().own?.guestLink).toBe(false); await revoked;
    screen.handleEvent({ ev: "screen.link", d: { share: "M", active: false } });
    screen.setGuestLink(true); // A revoked link cannot be re-enabled during recovery.
    screen.handleEvent({ ev: "screen.stopped", d: { share: "M", reason } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(reason === "transport" ? 2 : 1);
    if (reason === "transport") {
      expect(calls.filter(([op]) => op === "screen.start")[1]![1].guest_link).toBe(false);
      expect(source.stop).not.toHaveBeenCalled(); expect(toast).not.toHaveBeenCalled();
    } else expect(source.stop).toHaveBeenCalledOnce();
    screen.reset();
  });

  it("explicit Stop during media-first recovery retires the old share quietly", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); const source = choosing(); await screen.confirmShare();
    media.publishers[0]!.params.state("failed"); await screen.stopShare();
    expect(calls.filter(([op]) => op === "screen.stop")).toEqual([["screen.stop", { share: "M" }]]);
    screen.handleEvent({ ev: "screen.stopped", d: { share: "M", reason: "transport" } });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    expect(source.stop).toHaveBeenCalledOnce(); expect(useScreen.getState().notice).toBeNull(); screen.reset();
  });

  it("retains capture and an active link when the server reports media loss first", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); vi.spyOn(screen, "guestLinkPossible").mockReturnValue(true);
    const source = choosing(); await screen.confirmShare(); screen.handleEvent({ ev: "screen.link", d: { share: "M", active: true } });
    screen.handleEvent({ ev: "screen.stopped", d: { share: "M", reason: "transport" } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(2);
    expect(calls.filter(([op]) => op === "screen.start")[1]![1].guest_link).toBe(true);
    expect(source.stop).not.toHaveBeenCalled(); expect(useScreen.getState().notice).toBeNull(); screen.reset();
  });

  it("bounds media-first recovery from the first failure without extending it on signalling loss", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); const source = choosing(); await screen.confirmShare();
    media.publishers[0]!.params.state("failed"); await vi.advanceTimersByTimeAsync(15_000); screen.suspend();
    await vi.advanceTimersByTimeAsync(29_999); expect(source.stop).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1); expect(source.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().notice).toBe("reconnect"); screen.reset();
  });

  it.each(["permission", "permissionHint"])("replaces a sticky stopped toast with %s guidance", async notice => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); choosing(); await screen.confirmShare();
    media.publishers[0]!.params.encodeFailed(); await flush();
    const options = toast.mock.calls[0]![2];
    useUi.setState({ toasts: [{ id: 1, kind: "error", text: "stopped", ...options }] });
    vi.stubGlobal("navigator", { userAgent: "Firefox/153", mediaDevices: { getDisplayMedia: async () => { throw new DOMException(notice === "permission" ? "Permission denied by system" : "Not allowed by the user agent or the platform", "NotAllowedError"); } } });
    options.action.run(); await flush();
    expect(useScreen.getState().notice).toBe(notice); expect(useUi.getState().toasts).toEqual([]);
    await vi.advanceTimersByTimeAsync(60_000); expect(useScreen.getState().notice).toBe(notice); screen.reset();
  });

  it("retains capture through repeated suspend and republishes after the new voice binding", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    let starts = 0;
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: `M${++starts}` }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String), track = choosing();
    await screen.confirmShare();
    const stream = useScreen.getState().own!.stream;
    screen.handleEvent({ ev: "screen.started", d: share("M1", 1, 1) });
    screen.suspend(); screen.suspend();
    expect(track.stop).not.toHaveBeenCalled();
    expect(useScreen.getState().own).toMatchObject({ phase: "reconnecting", stream, share: null });
    expect(media.publishers[0]!.close).toHaveBeenCalledOnce();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, 1, { voice_binding: { call: "new-call", participation: "new-participation", run: 2 } }) } });
    screen.resume(); await flush();
    expect(calls.filter(([op]) => op === "screen.start")[1]![1]).toMatchObject({ call: "new-call", participation: "new-participation" });
    expect(useScreen.getState().own?.stream).toBe(stream);
    expect(media.publishers).toHaveLength(2);
    screen.handleEvent({ ev: "screen.started", d: share("M2", 1, 1) });
    expect(useScreen.getState().own?.phase).toBe("live");
    expect(useScreen.getState().notice).toBeNull();
    screen.reset();
    expect(track.stop).toHaveBeenCalledOnce();
  });

  it.each(["signalling", "media"])("retires %s uplink measurements while retaining capture, and fences old callbacks after republishing", async loss => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    let starts = 0;
    const { link } = fakeLink({ "screen.start": () => ({ share: `M${++starts}` }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String), track = choosing();
    await screen.confirmShare();
    screen.handleEvent({ ev: "screen.started", d: share("M1", 1, 1) });
    const stream = useScreen.getState().own!.stream;
    const old = media.publishers[0]!;
    const bloated = { congested: true, rttMs: 480, baselineMs: 20, availableOutgoingBitrate: null };
    old.params.uplink!(bloated);
    expect(useUplink.getState().screen).toBe(bloated);
    if (loss === "signalling") screen.suspend();
    else old.params.state("failed");
    expect(useScreen.getState().own).toMatchObject({ phase: "reconnecting", stream });
    expect(track.stop).not.toHaveBeenCalled();
    expect(useUplink.getState().screen).toBeNull();
    old.params.uplink!(bloated);
    expect(useUplink.getState().screen).toBeNull();
    if (loss === "signalling") screen.resume();
    else screen.handleEvent({ ev: "screen.stopped", d: { share: "M1", reason: "transport" } });
    await flush();
    expect(media.publishers).toHaveLength(2);
    expect(useScreen.getState().own?.stream).toBe(stream);
    const fresh = { ...bloated, congested: false, rttMs: 20 };
    media.publishers[1]!.params.uplink!(fresh);
    old.params.uplink!(null);
    old.params.uplink!(bloated);
    expect(useUplink.getState().screen).toBe(fresh);
    screen.reset();
    expect(useUplink.getState().screen).toBeNull();
  });

  it("waits for voice rejoin after Welcome before republishing", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); choosing();
    const subscribe = useSession.subscribe;
    let unsubscribe = () => {};
    const spy = vi.spyOn(useSession, "subscribe").mockImplementation(listener => { unsubscribe = subscribe(listener); return unsubscribe; });
    screen.init();
    try {
      await screen.confirmShare(); screen.suspend();
      useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
      screen.resume(); await flush();
      expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
      useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, 1) } });
      await flush();
      expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(2);
      expect(useScreen.getState().own?.phase).toBe("starting");
    } finally { unsubscribe(); spy.mockRestore(); screen.reset(); }
  });

  it("retains the original channel across a second blip before voice rejoin", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen); choosing();
    await screen.confirmShare(); screen.suspend();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
    screen.resume(); screen.suspend(); screen.resume();
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, 1) } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(2);
    expect(useScreen.getState().notice).toBeNull();
    screen.reset();
  });

  it.each(["encrypted", "different"])("does not republish into a %s channel after resync", async change => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen); const track = choosing();
    await screen.confirmShare(); screen.suspend();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
    screen.resume();
    if (change === "encrypted") useSession.setState({ channels: { ...useSession.getState().channels, 1: ch(1, true) } });
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, change === "different" ? 2 : 1) } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    expect(track.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().notice).toBe("reconnect");
    screen.reset();
  });

  it("tells an unconfirmed sharer when resync puts them in a different channel", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({});
    const screen = new ScreenController(() => link, String); initialise(screen); const source = choosing();
    screen.suspend();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
    screen.resume();
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, 2) } });
    expect(source.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().notice).toBe("reconnect");
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(0);
    screen.reset();
  });

  it.each(["picker", "choosing"])("keeps a %s capture across suspend and queues its confirmation", async phase => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const picked = deferred<MediaStream>();
    vi.stubGlobal("navigator", { mediaDevices: { getDisplayMedia: () => picked.promise } });
    const source = track(); const stream = { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream;
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen);
    screen.beginShare();
    if (phase === "choosing") { picked.resolve(stream); await flush(); }
    screen.suspend();
    if (phase === "picker") { picked.resolve(stream); await flush(); }
    expect(useScreen.getState().own).toMatchObject({ phase: "choosing", stream });
    expect(source.stop).not.toHaveBeenCalled();
    if (phase === "choosing") {
      screen.resume(); await flush();
      expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(0);
      screen.suspend();
    }
    await screen.confirmShare();
    expect(useScreen.getState().own?.phase).toBe("reconnecting");
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(0);
    screen.resume(); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    screen.reset();
  });

  it.each([false, true])("shows persistent guidance when the captured window ends during recovery (resumed=%s)", async resumed => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const source = Object.assign(new EventTarget(), { readyState: "live", stop: vi.fn(), getSettings: () => ({ displaySurface: "browser" }) });
    const stream = { getTracks: () => [source], getVideoTracks: () => [source] } as unknown as MediaStream;
    vi.stubGlobal("navigator", { mediaDevices: { getDisplayMedia: async () => stream } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String);
    screen.beginShare(); await flush(); await screen.confirmShare(); screen.suspend();
    if (resumed) { screen.resume(); await flush(); }
    const startsBeforeEnding = calls.filter(([op]) => op === "screen.start").length;
    source.readyState = "ended"; source.dispatchEvent(new Event("ended")); await flush(); screen.resume();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(useScreen.getState().own).toBeNull();
    expect(useScreen.getState().notice).toBe("reconnect");
    expect(toast).not.toHaveBeenCalled();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(startsBeforeEnding);
    screen.reset();
  });

  it("queues retained quality confirmation after Welcome until the new voice binding arrives", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); initialise(screen); const source = choosing();
    screen.suspend();
    useSession.getState().dispatch({ type: "event", now: 3, event: { ev: "client.updated", d: cl(1, null) } });
    screen.resume(); await screen.confirmShare();
    const spec = useScreen.getState().own!.spec;
    expect(useScreen.getState().own).toMatchObject({ phase: "reconnecting", spec });
    expect(source.stop).not.toHaveBeenCalled(); expect(calls).toEqual([]);
    useSession.getState().dispatch({ type: "event", now: 4, event: { ev: "client.updated", d: cl(1, 1, { voice_binding: null }) } });
    await flush(); expect(calls).toEqual([]);
    useSession.getState().dispatch({ type: "event", now: 5, event: { ev: "client.updated", d: cl(1, 1) } }); await flush();
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    expect(useScreen.getState().own?.spec).toEqual(spec); screen.reset();
  });

  it("passes learned encoder fallbacks into a republish of the same capture", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); choosing(); await screen.confirmShare();
    const learned = { startHintUnsafe: true, brokenCodecs: ["h264" as const], codecFellBack: true, av1Retired: false, recovery: { share: "M", codec: "vp9" as const, width: 1280, height: 720, fps: 30 } };
    vi.mocked(media.publishers[0]!.fallbacks).mockReturnValue(learned);
    screen.suspend(); screen.resume(); await flush();
    expect(media.publishers[1]!.params.fallbacks).toEqual(learned);
    screen.reset();
    choosing(); await screen.confirmShare();
    expect(media.publishers[2]!.params.fallbacks).toBeUndefined();
    screen.reset();
  });

  it.each(["request", "capture", "timeout"])("keeps a persistent notice when recovery fails: %s", async failure => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    let fail = false;
    const { link } = fakeLink({ "screen.start": () => { if (fail) throw new RequestError("busy", "full"); return { share: "M" }; }, "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String), track = choosing();
    await screen.confirmShare();
    screen.suspend();
    fail = true;
    if (failure === "capture") track.readyState = "ended";
    if (failure === "timeout") await vi.advanceTimersByTimeAsync(45_000);
    else { screen.resume(); await flush(); }
    expect(useScreen.getState().own).toBeNull();
    expect(track.stop).toHaveBeenCalledOnce();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(useScreen.getState().notice).toBe("reconnect");
    expect(toast).not.toHaveBeenCalled();
    screen.reset();
  });

  it("stops capture immediately but awaits explicit Stop before closing DTLS", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const order: string[] = [];
    const acknowledgement = deferred<object>();
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }), "screen.stop": () => { order.push("stop request"); return acknowledgement.promise; } });
    const screen = new ScreenController(() => link, String), track = choosing();
    await screen.confirmShare();
    vi.mocked(media.publishers[0]!.close).mockImplementation(() => { order.push("close publisher"); });
    const stopping = screen.stopShare();
    expect(track.stop).toHaveBeenCalledOnce();
    expect(useScreen.getState().own).toBeNull();
    expect(media.publishers[0]!.close).not.toHaveBeenCalled();
    acknowledgement.resolve({}); await stopping;
    expect(order).toEqual(["stop request", "close publisher"]);
    screen.reset();
  });

  it("explicit Stop during reconnect prevents republishing and does not show a failure notice", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); choosing();
    await screen.confirmShare();
    screen.suspend(); await screen.stopShare(); screen.resume();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(calls.filter(([op]) => op === "screen.start")).toHaveLength(1);
    expect(useScreen.getState().notice).toBeNull();
    screen.reset();
  });

  it("fences a late start response from before a reconnect", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const old = deferred<{ share: string }>();
    let starts = 0;
    const { link } = fakeLink({ "screen.start": () => ++starts === 1 ? old.promise : { share: "new" }, "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String); choosing();
    const starting = screen.confirmShare(); await flush();
    screen.suspend(); screen.resume(); await flush();
    old.resolve({ share: "old" }); await starting;
    expect(useScreen.getState().own?.share).toBe("new");
    expect(media.publishers).toHaveLength(1);
    screen.reset();
  });

  it("stops a silently ended capture through the normal path and ignores an old publisher", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String), t = choosing();
    await screen.confirmShare();
    const publisher = media.publishers[0]!;
    publisher.params.ended(); await flush();
    expect(useScreen.getState().own).toBeNull();
    expect(t.stop).toHaveBeenCalledTimes(1);
    expect(publisher.close).toHaveBeenCalledTimes(1);
    expect(calls.filter(([op]) => op === "screen.stop")).toEqual([["screen.stop", { share: "M" }]]);
    expect(toast).not.toHaveBeenCalled();
    choosing(); await screen.confirmShare();
    publisher.params.ended(); await flush();
    expect(useScreen.getState().own?.share).toBe("M");
    await screen.stopShare();
  });

  it("stops a share whose encoder never produced a frame, with one toast, and ignores an old publisher", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String), t = choosing();
    await screen.confirmShare();
    const publisher = media.publishers[0]!;
    publisher.params.encodeFailed(); await flush();
    expect(useScreen.getState().own).toBeNull();
    expect(t.stop).toHaveBeenCalledTimes(1);
    expect(publisher.close).toHaveBeenCalledTimes(1);
    expect(calls.filter(([op]) => op === "screen.stop")).toEqual([["screen.stop", { share: "M" }]]);
    expect(toast).toHaveBeenCalledExactlyOnceWith("error", "Your screen share was stopped: this browser could not encode the video.", expect.objectContaining({ key: "screen-stopped", sticky: true, action: { label: "Share again", run: expect.any(Function) } }));
    choosing(); await screen.confirmShare();
    publisher.params.encodeFailed(); await flush();
    expect(useScreen.getState().own?.share).toBe("M");
    expect(toast).toHaveBeenCalledTimes(1);
    await screen.stopShare();
  });

  it.each(["modern", "legacy", "unsupported"])("sends all quality choices safely to a %s server", async server => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    for (const mode of ["smooth", "sharp"] as const) for (const height of [720, 1080, 1440] as const) for (const fps of [15, 30, 60] as const) {
      const spec = { mode, height, fps };
      const { link, calls } = fakeLink({
        "screen.spec.capabilities": () => { if (server === "unsupported") throw new Error("unknown operation"); return { screen_spec: server === "modern" }; },
        "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }),
      });
      const screen = new ScreenController(() => link, String);
      choosing(); useScreen.setState({ own: { ...useScreen.getState().own!, spec } });
      await screen.confirmShare();
      const sent = calls.find(([op]) => op === "screen.start")![1];
      expect(sent.preset).toBe(mode === "sharp" ? "text" : fps === 60 ? "game" : height === 720 ? "saver" : "smooth");
      if (server === "modern") expect(sent.spec).toEqual(spec);
      else expect(Object.hasOwn(sent, "spec")).toBe(false);
      expect(media.publishers.at(-1)!.params).toMatchObject({ quality: spec, legacy: server !== "modern", automaticMode: true });
      const { applyCaptureSpec } = await import("../voice/screen");
      expect(applyCaptureSpec).toHaveBeenLastCalledWith(expect.anything(), spec, server !== "modern");
      await screen.stopShare();
    }
  });

  it("starts after the preset menu and publishes on the share's own transport", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String);
    choosing();
    await screen.confirmShare();
    expect(calls.map(([op]) => op)).toEqual(["screen.start", "screen.publish.offer"]);
    expect(calls[0]![1]).toEqual({ call: "AAAAAAAAAAAAAAAAAAAAAA", participation: "AQEBAQEBAQEBAQEBAQEBAQ", preset: "text", surface: "tab", guest_link: false });
    expect(calls[1]![1]).toMatchObject({ share: "M", ssrc: 1, rtx_ssrc: 2 });
    screen.handleEvent({ ev: "screen.started", d: share("M", 1, 1) });
    expect(useScreen.getState().own?.phase).toBe("live");
    screen.handleEvent({ ev: "screen.viewers", d: { share: "M", members: 2, guests: 0 } });
    expect(useScreen.getState().own?.viewers).toBe(2);
  });

  it("starts with the preset chosen while screen.start was pending", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const started = deferred<{ share: string }>();
    const { link } = fakeLink({ "screen.start": () => started.promise, "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String);
    const t = choosing();
    const confirming = screen.confirmShare();
    await flush();
    screen.setQuality(legacySpec("saver"));
    started.resolve({ share: "M" });
    await confirming;
    expect(media.publishers[0]!.params.quality).toEqual(legacySpec("saver"));
    const { applyCaptureSpec } = await import("../voice/screen");
    expect(vi.mocked(applyCaptureSpec).mock.calls.at(-1)).toEqual([t, legacySpec("saver"), true]);
  });

  it("keeps automatic intent through size/fps edits and makes an equal mode click explicit", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String);
    choosing();
    await screen.confirmShare();
    const publisher = media.publishers.at(-1)!;
    screen.setQuality({ height: 1440, fps: 60 });
    expect(publisher.setSpec).toHaveBeenLastCalledWith({ mode: "sharp", height: 1440, fps: 60 }, true);
    publisher.params.surfaceChanged("screen", { mode: "smooth", height: 1440, fps: 60 });
    expect(useScreen.getState().own).toMatchObject({ surface: "screen", spec: { mode: "smooth", height: 1440, fps: 60 } });
    expect(useSettings.getState().screenMode).toBeNull();
    screen.setQuality({ mode: "smooth" });
    expect(publisher.setSpec).toHaveBeenLastCalledWith({ mode: "smooth", height: 1440, fps: 60 }, false);
    expect(useSettings.getState().screenMode).toBe("smooth");
    await screen.stopShare();
    publisher.params.surfaceChanged("window", legacySpec("text"));
    expect(useScreen.getState().own).toBeNull();
  });

  it("refuses in an encrypted channel without sending anything", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 3) } });
    const { link, ops } = fakeLink({});
    const screen = new ScreenController(() => link, String);
    expect(screen.availability()).toBe("encrypted");
    const t = choosing();
    await screen.confirmShare();
    expect(ops()).toEqual([]);
    expect(t.stop).toHaveBeenCalled();
    expect(useScreen.getState().own).toBeNull();
  });

  describe("TeamSpeak watch links", () => {
    const teamspeak = (patch: Record<string, unknown> = {}) => {
      const server = useSession.getState().server!;
      useSession.setState({ server: { ...server, teamspeak: true, file_access: "link", screen_guest_links: true, ...patch } });
    };
    const start = async (configure: (screen: ScreenController) => void = () => {}) => {
      useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
      const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
      const screen = new ScreenController(() => link, String);
      choosing();
      configure(screen);
      await screen.confirmShare();
      return { screen, start: calls.find(([op]) => op === "screen.start")![1] };
    };

    it("asks for a link only where one is possible and the sharer left the checkbox on", async () => {
      teamspeak();
      expect((await start()).start.guest_link).toBe(true);
      useScreen.setState({ ...initialScreen });
      expect((await start(screen => screen.setGuestLink(false))).start.guest_link).toBe(false);
    });

    it.each(["sharer", "moderator"])("does not recreate a watch link revoked by the %s", async who => {
      teamspeak();
      useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
      const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
      const screen = new ScreenController(() => link, String); choosing(); await screen.confirmShare();
      screen.handleEvent({ ev: "screen.started", d: share("M", 1, 1) });
      screen.handleEvent({ ev: "screen.link", d: { share: "M", active: true } });
      if (who === "sharer") await screen.turnOffLink("M");
      else screen.handleEvent({ ev: "screen.link", d: { share: "M", active: false } });
      expect(useScreen.getState().own?.guestLink).toBe(false);
      screen.suspend(); screen.resume(); await flush();
      expect(calls.filter(([op]) => op === "screen.start").at(-1)![1].guest_link).toBe(false);
      screen.reset();
    });

    it("retains my revoke choice when the connection drops before its acknowledgement", async () => {
      teamspeak();
      useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
      const revoked = deferred<object>();
      const { link, calls } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }), "screen.link.revoke": () => revoked.promise });
      const screen = new ScreenController(() => link, String); choosing(); await screen.confirmShare();
      screen.handleEvent({ ev: "screen.started", d: share("M", 1, 1) });
      screen.handleEvent({ ev: "screen.link", d: { share: "M", active: true } });
      const revoking = screen.turnOffLink("M");
      screen.suspend(); screen.resume(); await flush();
      expect(calls.filter(([op]) => op === "screen.start").at(-1)![1].guest_link).toBe(false);
      revoked.resolve({}); await revoking; screen.reset();
    });

    it("never asks without a bridged channel, links for files or the server's watch links", async () => {
      for (const patch of [{ teamspeak: false }, { file_access: "members" }, { screen_guest_links: false }]) {
        useScreen.setState({ ...initialScreen });
        teamspeak(patch);
        expect((await start()).start.guest_link, JSON.stringify(patch)).toBe(false);
      }
      teamspeak();
      useSession.setState({ channels: { ...useSession.getState().channels, 1: { ...ch(1), private: true } } });
      useScreen.setState({ ...initialScreen });
      expect((await start()).start.guest_link, "private channel").toBe(false);
    });

    it("shows the link and its guests, and forgets it when it is off or the share ends", async () => {
      teamspeak();
      const { screen } = await start();
      screen.handleEvent({ ev: "screen.started", d: share("M", 1, 1) });
      screen.handleEvent({ ev: "screen.link", d: { share: "M", active: true } });
      screen.handleEvent({ ev: "screen.viewers", d: { share: "M", members: 1, guests: 2 } });
      expect(useScreen.getState().links).toEqual({ M: true });
      expect(useScreen.getState().own).toMatchObject({ viewers: 1, guests: 2 });
      screen.handleEvent({ ev: "screen.link", d: { share: "M", active: false } });
      expect(useScreen.getState().links).toEqual({});
      // A moderator hears of someone else's link; it goes with the share.
      screen.handleEvent({ ev: "screen.link", d: { share: "A", active: true } });
      screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "stopped" } });
      expect(useScreen.getState().links).toEqual({});
    });
  });

  it("ends my share when a moderator stops it", async () => {
    useSession.getState().dispatch({ type: "event", now: 2, event: { ev: "client.updated", d: cl(1, 1) } });
    const { link } = fakeLink({ "screen.start": () => ({ share: "M" }), "screen.publish.offer": () => ({ sdp: "answer" }) });
    const screen = new ScreenController(() => link, String);
    const t = choosing();
    await screen.confirmShare();
    screen.handleEvent({ ev: "screen.stopped", d: { share: "M", reason: "moderator", by: "Mod" } });
    expect(useScreen.getState().own).toBeNull();
    expect(t.stop).toHaveBeenCalled();
    expect(media.publishers[0]!.close).toHaveBeenCalled();
    const begin = vi.spyOn(screen, "beginShare").mockImplementation(() => {});
    toast.mock.calls.at(-1)![2].action.run();
    expect(begin).toHaveBeenCalledOnce();
    expect(toast).toHaveBeenCalledWith("info", "Mod stopped your screen share.", expect.objectContaining({ sticky: true, key: "screen-stopped" }));
  });
});
