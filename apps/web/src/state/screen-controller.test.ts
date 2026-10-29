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

interface PublisherOptions {
  ended(): void;
  quality: ScreenSpec; legacy: boolean; automaticMode: boolean;
  surfaceChanged(surface: "screen" | "window" | "tab" | "unknown", spec: ScreenSpec): void;
  offer(body: object): Promise<string>; state(s: string): void; encodeFailed(): void;
}

const media = vi.hoisted(() => ({
  transports: [] as Array<{ params: { run: number; state(s: string): void; offer(sdp: string, run: number): Promise<string> }; close: () => void }>,
  publishers: [] as Array<{ params: PublisherOptions; close: () => void; setSpec: (spec: ScreenSpec, automaticMode: boolean) => Promise<void> }>,
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
afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); });

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
    else screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 1, share: null } });
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
    else screen.handleEvent({ ev: "screen.slot", d: { run: old.params.run, slot: 1, share: null } });
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

  it("ends waiting watches through the existing transport failure path", async () => {
    const { screen, start } = setup();
    await start(false);
    await screen.unwatch("B");
    const watching = screen.watch("C");
    await flush();
    media.transports[1]!.params.state("failed");
    await watching;
    await flush();
    expect(useScreen.getState().watches).toEqual([]);
    expect(useScreen.getState().stage).toBeNull();
    expect(toast).toHaveBeenCalledExactlyOnceWith("error", "The connection for watching screen shares was lost.");
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
    expect(vi.getTimerCount()).toBe(0);
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
    const watchingB = screen.watch("B");
    await flush();
    media.transports[1]!.params.state("connected");
    await vi.advanceTimersByTimeAsync(6_000);
    await Promise.all([watchingA, watchingB]);
    const asksForA = calls.filter(([op, d]) => op === "screen.watch" && d.share === "A").length;
    await vi.advanceTimersByTimeAsync(6_000);
    expect(calls.filter(([op, d]) => op === "screen.watch" && d.share === "A").length).toBe(asksForA);
    expect(useScreen.getState().watches).toEqual([{ share: "B", id: expect.any(Number), slot: 0, status: "watching" }]);
    expect(toast.mock.calls).toEqual([["error", "The connection for watching screen shares was lost."]]);
  });

  it("a slot granted to a retired watch is released", async () => {
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
    expect(ops()).toEqual(["screen.watch.offer", "screen.watch", "screen.unwatch"]);
    expect(useScreen.getState().watches).toEqual([]);
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
    expect(useScreen.getState().watches).toEqual([]);
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "access" } });
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
      expect(useScreen.getState()).toMatchObject({ watches: [], stage: null });
      expect(toast).toHaveBeenCalledTimes(1);
      expect(toast).toHaveBeenCalledWith("error", "The connection for watching screen shares was lost.");
    }
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
  const track = () => ({ stop: vi.fn(), addEventListener: vi.fn(), getSettings: () => ({ displaySurface: "browser" }) });
  const choosing = (t = track()) => {
    useScreen.setState({ own: { phase: "choosing", attempt: 1, share: null, stream: { getVideoTracks: () => [t], getTracks: () => [t] } as unknown as MediaStream, surface: "tab", spec: legacySpec("text"), viewers: 0, guests: 0, guestLink: true, preview: false } });
    return t;
  };

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
    const unexpected = vi.fn();
    const screen = new ScreenController(() => link, String, unexpected), t = choosing();
    await screen.confirmShare();
    const publisher = media.publishers[0]!;
    publisher.params.encodeFailed(); await flush();
    expect(useScreen.getState().own).toBeNull();
    expect(unexpected).toHaveBeenCalledOnce();
    expect(t.stop).toHaveBeenCalledTimes(1);
    expect(publisher.close).toHaveBeenCalledTimes(1);
    expect(calls.filter(([op]) => op === "screen.stop")).toEqual([["screen.stop", { share: "M" }]]);
    expect(toast).toHaveBeenCalledExactlyOnceWith("error", "Your screen share was stopped: this browser could not encode the video.");
    choosing(); await screen.confirmShare();
    publisher.params.encodeFailed(); await flush();
    expect(useScreen.getState().own?.share).toBe("M");
    expect(toast).toHaveBeenCalledTimes(1);
    await screen.stopShare();
    expect(unexpected).toHaveBeenCalledOnce();
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
    expect(toast).toHaveBeenCalledWith("info", "Mod stopped your screen share.");
  });
});
