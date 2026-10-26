import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { pageContextFromLocation } from "../net/address";
import type { WatchParams } from "../voice/screen";
import { PING_INTERVAL_MS, readToken, watchEndpoint, WatchSession, type SocketLike, type WatchStatus } from "./session";
import { watchText } from "./text";

const TOKEN = "A".repeat(42) + "_";

class FakeSocket implements SocketLike {
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  readyState = 0;
  sent: Array<{ id: number; op: string; d: Record<string, unknown> }> = [];
  closed = false;
  constructor(readonly url: string) {}
  send(data: string) { this.sent.push(JSON.parse(data)); }
  close() { this.closed = true; this.readyState = 3; }
  open() { this.readyState = 1; this.onopen?.(new Event("open")); }
  receive(frame: unknown) { this.onmessage?.({ data: JSON.stringify(frame) } as MessageEvent); }
  last(op: string) { return [...this.sent].reverse().find(f => f.op === op); }
}

function setup() {
  const statuses: WatchStatus[] = [];
  const names: string[] = [];
  const streams: MediaStream[] = [];
  let socket: FakeSocket | null = null;
  let params: WatchParams | null = null;
  const transport = { start: vi.fn(async () => { await params!.offer("v=0 offer", params!.run); }), close: vi.fn() };
  const session = new WatchSession(
    { status: s => statuses.push(s), sharer: n => names.push(n), stream: s => streams.push(s) },
    { socket: url => (socket = new FakeSocket(url)), transport: p => { params = p; return transport; } },
  );
  return { session, statuses, names, streams, transport, socket: () => socket!, params: () => params! };
}

const watching = { share: "s1", sharer: "alice", preset: "smooth", ice_servers: [{ urls: ["stun:127.0.0.1:3478"] }] };

beforeEach(() => {
  vi.useFakeTimers();
  vi.stubGlobal("RTCPeerConnection", class {});
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("watch page endpoint and token", () => {
  const page = { protocol: "https:", hostname: "voice.example.com", port: "" };

  it("uses this page's server, or the link's server= endpoint, on /ws/watch", () => {
    expect(watchEndpoint("", page)).toBe("wss://voice.example.com/ws/watch");
    expect(watchEndpoint(`?server=${encodeURIComponent("wss://other.example:8443/ws")}`, { ...page, hostname: "gwar.app" })).toBe("wss://other.example:8443/ws/watch");
    expect(watchEndpoint("", { protocol: "http:", hostname: "127.0.0.1", port: "8790" })).toBe("ws://127.0.0.1:8790/ws/watch");
    // A secure page never talks to a plaintext remote server.
    expect(watchEndpoint(`?server=${encodeURIComponent("ws://other.example/ws")}`, page)).toBeNull();
  });

  it("opens a same-origin IPv6 page's server, as the browser names its host", () => {
    for (const href of ["http://[::1]:8790/watch", "https://[2001:db8::7]/watch"]) {
      const url = new URL(href);
      const expected = `${url.protocol === "https:" ? "wss" : "ws"}://${url.host}/ws/watch`;
      expect(watchEndpoint("", pageContextFromLocation(url))).toBe(expected);
    }
  });

  it("takes only a 43-character base64url token from the fragment", () => {
    expect(readToken(`#${TOKEN}`)).toBe(TOKEN);
    expect(readToken("")).toBeNull();
    expect(readToken(`#${TOKEN}x`)).toBeNull();
    expect(readToken(`#${"A".repeat(42)}+`)).toBeNull();
  });

  it("speaks Polish for pl and English otherwise", () => {
    expect(watchText("pl-PL").status.ended).toBe("Udostępnianie ekranu się zakończyło");
    expect(watchText("en-US").status.ended).toBe("The share has ended");
    expect(watchText(undefined).lang).toBe("en");
  });
});

describe("WatchSession", () => {
  it("bounds unanswered admission and clears request timers when closed", async () => {
    const t = setup();
    t.session.start("ws://127.0.0.1/ws/watch", TOKEN);
    t.socket().open();
    await vi.advanceTimersByTimeAsync(35_000);
    expect(t.session.status).toBe("lost");
    expect(t.socket().closed).toBe(true);
    expect(vi.getTimerCount()).toBe(0);
    const u = setup();
    u.session.start("ws://127.0.0.1/ws/watch", TOKEN);
    u.socket().open();
    expect(vi.getTimerCount()).toBe(1);
    u.session.close();
    await vi.advanceTimersByTimeAsync(0);
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each([false,true])("gates guest extensions on the server advertisement (%s)", async supported => {
    const t = setup();
    t.session.start("ws://127.0.0.1:1/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: { ...watching, ...(supported ? { screen_h264: true } : {}) } });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    expect(t.socket().last("guest.offer")!.d).toEqual(supported ? { sdp: "v=0 offer", browser: true, h264: [] } : { sdp: "v=0 offer" });
    t.params().decodeFailed?.(0,"s1",1);
    expect(t.socket().last("screen.decode_failed") !== undefined).toBe(supported);
    t.session.close();
  });
  it("watches through one receive-only slot and pings every 20 s once admitted", async () => {
    const t = setup();
    t.session.start("ws://127.0.0.1:1/ws/watch", TOKEN);
    const socket = t.socket();
    socket.open();
    expect(socket.sent).toEqual([{ id: 1, op: "guest.watch", d: { token: TOKEN } }]);
    // No ping before the server admits the guest.
    vi.advanceTimersByTime(PING_INTERVAL_MS);
    expect(socket.last("ping")).toBeUndefined();
    socket.receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(socket.last("guest.offer")).toBeDefined());
    expect(t.names).toEqual(["alice"]);
    expect(t.params().slots).toBe(1);
    expect(socket.last("guest.offer")!.d).toEqual({ sdp: "v=0 offer" });
    socket.receive({ re: socket.last("guest.offer")!.id, ok: { sdp: "v=0 answer" } });
    t.params().state("connected");
    expect(t.statuses).toEqual(["watching"]);
    vi.advanceTimersByTime(PING_INTERVAL_MS * 3);
    expect(socket.sent.filter(f => f.op === "ping")).toHaveLength(3);
    expect(socket.sent.filter(f => f.op === "guest.offer")).toHaveLength(1);
  });

  it("shows the end of the share, not a lost connection, when the server then closes", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    // Another share's events are not this page's business.
    t.socket().receive({ ev: "screen.stopped", d: { share: "s2", reason: "stopped" } });
    expect(t.session.status).toBe("connecting");
    const socket = t.socket();
    socket.receive({ ev: "screen.stopped", d: { share: "s1", reason: "stopped" } });
    socket.onclose?.({} as CloseEvent);
    expect(t.statuses).toEqual(["ended"]);
    expect(t.transport.close).toHaveBeenCalled();
    expect(socket.closed).toBe(true);
    // Nothing pings a closed session.
    const sent = socket.sent.length;
    vi.advanceTimersByTime(PING_INTERVAL_MS * 2);
    expect(socket.sent.length).toBe(sent);
  });

  it("keeps the explicit codec refusal when the guest socket closes", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    const socket = t.socket();
    socket.receive({ ev: "screen.stopped", d: { share: "s1", reason: "codec" } });
    socket.onclose?.({} as CloseEvent);
    expect(t.session.status).toBe("codec");
    expect(t.transport.close).toHaveBeenCalled();
  });

  it("says the link was turned off", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    t.socket().receive({ ev: "screen.link", d: { share: "s1", active: false } });
    expect(t.statuses).toEqual(["off"]);
  });

  it("tells a slow connection apart and recovers when the share resumes", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    t.params().state("connected");
    t.socket().receive({ ev: "screen.paused", d: { share: "s1", reason: "bandwidth" } });
    t.socket().receive({ ev: "screen.resumed", d: { share: "s1" } });
    expect(t.statuses).toEqual(["watching", "slow", "watching"]);
  });

  it("keeps a budget pause across transport connection and waits for the matching resume", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(t.socket().last("guest.offer")).toBeDefined());
    t.socket().receive({ ev: "screen.paused", d: { share: "s1", reason: "budget" } });
    t.params().state("connected");
    t.socket().receive({ ev: "screen.resumed", d: { share: "s2" } });
    expect(t.session.status).toBe("paused");
    expect(t.transport.close).not.toHaveBeenCalled();
    expect(watchText("en").status.paused).toContain("Waiting to resume");
    expect(watchText("pl").status.paused).toContain("Oczekiwanie na wznowienie");
    t.socket().receive({ ev: "screen.resumed", d: { share: "s1" } });
    expect(t.statuses).toEqual(["paused", "watching"]);
  });

  it("answers not_found as a link that does not work, and busy as a full share", async () => {
    for (const [code, status] of [["not_found", "invalid"], ["busy", "busy"], ["rate_limited", "lost"]] as const) {
      const t = setup();
      t.session.start("ws://x/ws/watch", TOKEN);
      t.socket().open();
      const socket = t.socket();
      socket.receive({ re: 1, err: { code, message: "no" } });
      await vi.waitFor(() => expect(t.statuses).toEqual([status]));
      expect(socket.closed).toBe(true);
    }
  });

  it("never connects without a token or an endpoint", () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", null);
    expect(t.statuses).toEqual(["invalid"]);
    const u = setup();
    u.session.start(null, TOKEN);
    expect(u.statuses).toEqual(["invalid"]);
  });

  it("reports a lost connection or a failed transport", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().onclose?.({} as CloseEvent);
    expect(t.statuses).toEqual(["lost"]);
    const u = setup();
    u.session.start("ws://x/ws/watch", TOKEN);
    u.socket().open();
    u.socket().receive({ re: 1, ok: watching });
    await vi.waitFor(() => expect(u.socket().last("guest.offer")).toBeDefined());
    u.params().state("failed");
    expect(u.statuses).toEqual(["lost"]);
  });

  it("refuses a malformed admission", async () => {
    const t = setup();
    t.session.start("ws://x/ws/watch", TOKEN);
    t.socket().open();
    t.socket().receive({ re: 1, ok: { share: "s1", sharer: 5 } });
    await vi.waitFor(() => expect(t.statuses).toEqual(["lost"]));
    expect(t.transport.start).not.toHaveBeenCalled();
  });
});
