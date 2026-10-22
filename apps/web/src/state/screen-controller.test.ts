import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { RequestError, type Link } from "../net/connection";
import { welcome } from "../net/testing";
import type { Channel } from "../proto/Channel";
import type { Client } from "../proto/Client";
import { ScreenController } from "./screen-controller";
import { initialScreen, useScreen } from "./screen";
import { useSession, useUi } from "./stores";

const media = vi.hoisted(() => ({
  transports: [] as Array<{ params: { run: number; state(s: string): void; offer(sdp: string, run: number): Promise<string> }; close: () => void }>,
  publishers: [] as Array<{ params: { offer(body: object): Promise<string>; state(s: string): void }; close: () => void; setPreset: () => Promise<void> }>,
}));
vi.mock("../voice/screen", async importOriginal => {
  const actual = await importOriginal<typeof import("../voice/screen")>();
  class ScreenWatchTransport {
    readonly run: number;
    close = vi.fn();
    constructor(readonly params: { run: number; state(s: string): void; offer(sdp: string, run: number): Promise<string> }) { this.run = params.run; media.transports.push(this); }
    async start() { await this.params.offer("watch-offer", this.run); }
    async stats() { return null; }
  }
  class ScreenPublisher {
    close = vi.fn();
    setPreset = vi.fn(async () => {});
    constructor(readonly params: { offer(body: object): Promise<string>; state(s: string): void }) { media.publishers.push(this); }
    async start() { await this.params.offer({ ssrc: 1, rtx_ssrc: 2, sdp: "publish-offer" }); }
  }
  return { ...actual, ScreenWatchTransport, ScreenPublisher, applyCapturePreset: vi.fn(async () => {}), captureSupport: () => "ok" as const };
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
      calls.push([op, d]);
      const handler = handlers[op];
      if (!handler) return {};
      return handler(d);
    }),
  } as unknown as Link;
  return { link, calls, ops: () => calls.map(([op]) => op) };
}

const toast = vi.fn();
const flush = () => vi.advanceTimersByTimeAsync(0);

beforeEach(() => {
  vi.useFakeTimers();
  media.transports.length = 0;
  media.publishers.length = 0;
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
  it("watches without joining voice: one watch transport, then the slot, then focus", async () => {
    let unavailable = 2;
    const { link, ops, calls } = fakeLink({
      "screen.watch.offer": () => ({ sdp: "answer" }),
      "screen.watch": () => { if (unavailable-- > 0) throw new RequestError("unavailable", "connect a watch transport first"); return { slot: 1 }; },
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
    expect(useScreen.getState().watches).toEqual([{ share: "A", slot: 1, status: "watching" }]);
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
    useScreen.setState({ watches: ["W", "X", "Y", "Z"].map((share, slot) => ({ share, slot, status: "watching" as const })) });
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
    useScreen.setState({ watches: [{ share: "A", slot: 0, status: "watching" }], stage: "A" });
    screen.handleEvent({ ev: "screen.stopped", d: { share: "A", reason: "access" } });
    expect(useScreen.getState().stage).toBeNull();
    expect(toast).toHaveBeenCalledWith("info", "You can no longer watch this screen share.");
  });
});

describe("sharing", () => {
  const track = () => ({ stop: vi.fn(), addEventListener: vi.fn(), getSettings: () => ({ displaySurface: "browser" }) });
  const choosing = (t = track()) => {
    useScreen.setState({ own: { phase: "choosing", attempt: 1, share: null, stream: { getVideoTracks: () => [t], getTracks: () => [t] } as unknown as MediaStream, surface: "tab", choice: "auto", preset: "text", viewers: 0, preview: false } });
    return t;
  };

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
