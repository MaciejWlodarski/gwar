import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  backoffDelay,
  Connection,
  ConnectError,
  ConnectionLostError,
  RequestError,
  RequestTimeoutError,
  shouldReconnectAfter,
  type ConnectionOptions,
  type ConnectionStatus,
} from "./connection";
import { encodeRequest, parseFrame } from "./frames";
import { challengeMessage, generateIdentity } from "./identity";
import { decodeBase64Url } from "./base64url";
import { challenge, fakeIdentity, FakeSocket, welcome } from "./testing";

function make(overrides: Partial<ConnectionOptions> = {}) {
  return new Connection({
    url: "ws://test/ws",
    identity: fakeIdentity,
    nickname: "Ann",
    client: { name: "vc-web", version: "0.1.0", platform: "web" },
    createSocket: (u) => new FakeSocket(u),
    random: () => 0.5,
    ...overrides,
  });
}

/** Drives a successful handshake; returns the socket. */
async function handshake(conn: Connection, w = welcome()) {
  const p = conn.connect();
  const s = FakeSocket.last();
  s.open();
  s.push(challenge());
  await vi.waitFor(() => expect(s.sent.length).toBe(1));
  s.push({ re: 1, ok: w });
  await p;
  return s;
}

beforeEach(() => {
  FakeSocket.reset();
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval", "Date"] });
});
afterEach(() => vi.useRealTimers());

describe("framing", () => {
  it("encodes requests as {id, op, d}", () => {
    expect(JSON.parse(encodeRequest(7, "channel.join", { channel: 3 }))).toEqual({ id: 7, op: "channel.join", d: { channel: 3 } });
  });

  it("classifies server frames", () => {
    expect(parseFrame('{"re":3,"ok":{}}')).toEqual({ kind: "ok", re: 3, ok: {} });
    expect(parseFrame('{"re":3,"err":{"code":"forbidden","message":"no"}}')).toEqual({
      kind: "err",
      re: 3,
      err: { code: "forbidden", message: "no" },
    });
    expect(parseFrame('{"ev":"voice.talking","d":{"client":2,"talking":true}}')).toMatchObject({ kind: "event" });
    expect(parseFrame("nope")).toBeNull();
    expect(parseFrame('{"x":1}')).toBeNull();
  });
});

describe("signing message", () => {
  it("matches vc_proto::challenge_message", () => {
    expect(new TextDecoder().decode(challengeMessage("NONCE", "KEY"))).toBe("vc/1 hello\nNONCE\nKEY");
  });

  it("produces a signature the public key verifies", async () => {
    const id = await generateIdentity();
    expect(decodeBase64Url(id.publicKey)).toHaveLength(32);
    const msg = challengeMessage("n", id.publicKey);
    const sig = decodeBase64Url(await id.sign(msg));
    expect(sig).toHaveLength(64);
    const pub = await crypto.subtle.importKey("raw", decodeBase64Url(id.publicKey) as BufferSource, { name: "Ed25519" }, false, ["verify"]);
    expect(await crypto.subtle.verify({ name: "Ed25519" }, pub, sig as BufferSource, msg as BufferSource)).toBe(true);
  });
});

describe("handshake", () => {
  it("sends a signed hello after the challenge and resolves with Welcome", async () => {
    const conn = make();
    const s = await handshake(conn);
    const hello = s.sent[0] as { id: number; op: string; d: Record<string, unknown> };
    expect(hello.id).toBe(1);
    expect(hello.op).toBe("hello");
    expect(hello.d).toMatchObject({
      protocol: 1,
      nickname: "Ann",
      public_key: "PUBKEY",
      signature: "sig(vc/1 hello|abc|PUBKEY)",
      client: { name: "vc-web", version: "0.1.0", platform: "web" },
    });
    expect(hello.d).not.toHaveProperty("server_password");
    expect(conn.status.state).toBe("online");
  });

  it("sends the device certificate when signed in with Gwar Connect, and signs with the device key", async () => {
    const device = { account_key: "ACCOUNT", device_key: "PUBKEY", issued_at: 1, expires_at: 2, signature: "CERT" };
    const conn = make({ identity: { ...fakeIdentity, device } });
    const s = await handshake(conn);
    const d = (s.sent[0] as { d: Record<string, unknown> }).d;
    expect(d.public_key).toBe("PUBKEY");
    expect(d.signature).toBe("sig(vc/1 hello|abc|PUBKEY)");
    expect(d.device).toEqual(device);
  });

  it("sends no device field for a local identity", async () => {
    const s = await handshake(make());
    expect((s.sent[0] as { d: Record<string, unknown> }).d).not.toHaveProperty("device");
  });

  it("includes the server password when given", async () => {
    const conn = make({ serverPassword: "pw" });
    const p = conn.connect();
    const s = FakeSocket.last();
    s.open();
    s.push(challenge(true));
    await vi.waitFor(() => expect(s.sent.length).toBe(1));
    expect((s.sent[0] as { d: Record<string, unknown> }).d.server_password).toBe("pw");
    s.push({ re: 1, ok: welcome() });
    await p;
  });

  it("fails with password_required without sending hello", async () => {
    const conn = make();
    const p = conn.connect();
    const s = FakeSocket.last();
    s.open();
    s.push(challenge(true));
    await expect(p).rejects.toMatchObject({ kind: "password_required", serverName: "Test" });
    expect(s.sent).toHaveLength(0);
  });

  it.each([
    ["wrong_password", "wrong_password"],
    ["unavailable", "server_full"],
    ["bad_request", "rejected"],
  ])("maps hello error %s to %s", async (code, kind) => {
    const conn = make({ serverPassword: "x" });
    const p = conn.connect();
    p.catch(() => {});
    const s = FakeSocket.last();
    s.open();
    s.push(challenge(true));
    await vi.waitFor(() => expect(s.sent.length).toBe(1));
    s.push({ re: 1, err: { code, message: "m" } });
    await expect(p).rejects.toBeInstanceOf(ConnectError);
    await expect(p).rejects.toMatchObject({ kind });
  });

  it("reports unreachable when the socket closes early", async () => {
    const conn = make();
    const p = conn.connect();
    FakeSocket.last().drop();
    await expect(p).rejects.toMatchObject({ kind: "unreachable" });
    expect(conn.status).toMatchObject({ state: "closed", reason: { kind: "error" } });
  });

  it("times out a silent server", async () => {
    const conn = make({ handshakeTimeoutMs: 1000 });
    const p = conn.connect();
    p.catch(() => {});
    FakeSocket.last().open();
    vi.advanceTimersByTime(1001);
    await expect(p).rejects.toMatchObject({ kind: "timeout" });
  });

  it("rejects an unsupported protocol version", async () => {
    const conn = make();
    const p = conn.connect();
    const s = FakeSocket.last();
    s.open();
    const c = challenge();
    c.d.protocol = 2;
    s.push(c);
    await expect(p).rejects.toMatchObject({ kind: "protocol" });
  });
});

describe("requests", () => {
  it("correlates replies by id, out of order", async () => {
    const conn = make();
    const s = await handshake(conn);
    const a = conn.request("channel.join", { channel: 2 });
    const b = conn.request("ping", {});
    const [ra, rb] = s.sent.slice(1) as Array<{ id: number }>;
    expect(ra?.id).toBe(2);
    expect(rb?.id).toBe(3);
    s.push({ re: 3, ok: {} });
    await expect(b).resolves.toEqual({});
    s.push({ re: 2, err: { code: "wrong_password", message: "nope" } });
    await expect(a).rejects.toBeInstanceOf(RequestError);
    await expect(a).rejects.toMatchObject({ code: "wrong_password", message: "nope" });
  });

  it("times out", async () => {
    const conn = make();
    await handshake(conn);
    const p = conn.request("ping", {}, { timeoutMs: 500 });
    p.catch(() => {});
    vi.advanceTimersByTime(501);
    await expect(p).rejects.toBeInstanceOf(RequestTimeoutError);
  });

  it("rejects pending requests when the link drops and refuses new ones", async () => {
    const conn = make();
    const s = await handshake(conn);
    const p = conn.request("ping", {});
    p.catch(() => {});
    s.drop();
    await expect(p).rejects.toBeInstanceOf(ConnectionLostError);
    await expect(conn.request("ping", {})).rejects.toBeInstanceOf(ConnectionLostError);
  });

  it("dispatches typed events to subscribers and supports unsubscribe", async () => {
    const conn = make();
    const s = await handshake(conn);
    const seen: number[] = [];
    const off = conn.on("voice.talking", (d) => seen.push(d.client));
    s.push({ ev: "voice.talking", d: { client: 4, talking: true } });
    off();
    s.push({ ev: "voice.talking", d: { client: 5, talking: true } });
    expect(seen).toEqual([4]);
  });
});

describe("reconnect", () => {
  it("backs off with jitter and caps", () => {
    expect(backoffDelay(1, () => 0.5)).toBe(500);
    expect(backoffDelay(2, () => 0.5)).toBe(1000);
    expect(backoffDelay(3, () => 0.5)).toBe(2000);
    expect(backoffDelay(20, () => 0.5)).toBe(15000);
    expect(backoffDelay(1, () => 0)).toBe(375);
    expect(backoffDelay(1, () => 1)).toBe(625);
  });

  it("does not reconnect after kick or replacement", () => {
    expect(shouldReconnectAfter({ kind: "kicked", by: "a", reason: null })).toBe(false);
    expect(shouldReconnectAfter({ kind: "replaced" })).toBe(false);
    expect(shouldReconnectAfter({ kind: "removed", by: "a" })).toBe(false);
    expect(shouldReconnectAfter({ kind: "banned", by: "a", reason: null, until: null })).toBe(false);
    expect(shouldReconnectAfter({ kind: "server_shutdown" })).toBe(true);
    expect(shouldReconnectAfter({ kind: "timeout" })).toBe(true);
  });

  it("reconnects after a drop, retries failures, and resyncs from a new Welcome", async () => {
    const conn = make();
    const statuses: ConnectionStatus[] = [];
    conn.onStatus((st) => statuses.push(st));
    const welcomes: boolean[] = [];
    conn.onWelcome((_w, resync) => welcomes.push(resync));
    const s1 = await handshake(conn);
    expect(welcomes).toEqual([false]);

    s1.drop();
    expect(conn.status).toMatchObject({ state: "reconnecting", attempt: 1 });
    await vi.advanceTimersByTimeAsync(500);
    expect(FakeSocket.instances).toHaveLength(2);
    // First retry fails before the challenge.
    FakeSocket.last().drop();
    await vi.waitFor(() => expect(conn.status).toMatchObject({ state: "reconnecting", attempt: 2 }));
    await vi.advanceTimersByTimeAsync(1000);
    expect(FakeSocket.instances).toHaveLength(3);

    const s3 = FakeSocket.last();
    s3.open();
    s3.push(challenge());
    await vi.waitFor(() => expect(s3.sent.length).toBe(1));
    expect((s3.sent[0] as { id: number }).id).toBe(1);
    s3.push({ re: 1, ok: welcome({ session: 9 }) });
    await vi.waitFor(() => expect(conn.status.state).toBe("online"));
    expect(welcomes).toEqual([false, true]);

    // Requests work again with fresh ids.
    const p = conn.request("ping", {});
    expect((s3.sent[1] as { id: number }).id).toBe(2);
    s3.push({ re: 2, ok: {} });
    await p;
    expect(statuses.map((x) => x.state)).toEqual(["connecting", "online", "reconnecting", "reconnecting", "online"]);
  });

  it("stops for good when the server kicks us", async () => {
    const conn = make();
    const s = await handshake(conn);
    s.push({ ev: "disconnected", d: { reason: { kind: "kicked", by: "Admin", reason: "bye" } } });
    s.drop();
    expect(conn.status).toMatchObject({ state: "closed", reason: { kind: "server", reason: { kind: "kicked" } } });
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeSocket.instances).toHaveLength(1);
  });

  it("stops retrying when the server password changed", async () => {
    const conn = make({ serverPassword: "old" });
    const s1 = await (async () => {
      const p = conn.connect();
      const s = FakeSocket.last();
      s.open();
      s.push(challenge(true));
      await vi.waitFor(() => expect(s.sent.length).toBe(1));
      s.push({ re: 1, ok: welcome() });
      await p;
      return s;
    })();
    s1.drop();
    await vi.advanceTimersByTimeAsync(500);
    const s2 = FakeSocket.last();
    s2.open();
    s2.push(challenge(true));
    await vi.waitFor(() => expect(s2.sent.length).toBe(1));
    s2.push({ re: 1, err: { code: "wrong_password", message: "x" } });
    await vi.waitFor(() => expect(conn.status.state).toBe("closed"));
    expect(conn.status).toMatchObject({ reason: { kind: "error", error: { kind: "wrong_password" } } });
  });

  it("close() is final and does not reconnect", async () => {
    const conn = make();
    const s = await handshake(conn);
    conn.close();
    s.drop();
    await vi.advanceTimersByTimeAsync(60_000);
    expect(conn.status).toEqual({ state: "closed", reason: { kind: "user" } });
    expect(FakeSocket.instances).toHaveLength(1);
  });

  it("sends keepalive pings and reconnects when pongs stop", async () => {
    const conn = make({ pingIntervalMs: 1000 });
    const s = await handshake(conn);
    await vi.advanceTimersByTimeAsync(1000);
    expect(s.sent.at(-1)).toMatchObject({ op: "ping" });
    await vi.advanceTimersByTimeAsync(10_001);
    expect(conn.status.state).toBe("reconnecting");
  });
});

describe("identity helpers", () => {
  it("derives the same uid shape as the server (base64url of 20 bytes)", async () => {
    const { uidForPublicKey } = await import("./identity");
    const id = await generateIdentity();
    const uid = await uidForPublicKey(id.publicKey);
    expect(decodeBase64Url(uid)).toHaveLength(20);
  });

  it("round-trips a backup through import", async () => {
    const { importIdentity } = await import("./identity");
    const a = await generateIdentity();
    let saved: unknown;
    const b = await importIdentity(JSON.stringify(a.exportBackup()), { load: async () => undefined, save: async (x) => void (saved = x) });
    expect(b.publicKey).toBe(a.publicKey);
    expect(saved).toBeDefined();
    await expect(importIdentity("{}", { load: async () => undefined, save: async () => {} })).rejects.toThrow();
  });
});

describe("invites and bans", () => {
  it("sends the invite in hello, and only the first time", async () => {
    const conn = make({ invite: "CODE123" });
    const s1 = await handshake(conn);
    expect(s1.sent[0]).toMatchObject({ op: "hello", d: { invite: "CODE123" } });
    s1.drop();
    await vi.advanceTimersByTimeAsync(600);
    const s2 = FakeSocket.last();
    s2.open();
    s2.push(challenge());
    await vi.waitFor(() => expect(s2.sent.length).toBe(1));
    expect((s2.sent[0]?.d as Record<string, unknown>).invite).toBeUndefined();
    s2.push({ re: 1, ok: welcome() });
    conn.close();
  });

  it("omits the invite when there is none", async () => {
    const s = await handshake(make());
    expect((s.sent[0]?.d as Record<string, unknown>).invite).toBeUndefined();
  });

  it("an invite stands in for a required server password", async () => {
    const conn = make({ invite: "CODE123" });
    const p = conn.connect();
    const s = FakeSocket.last();
    s.open();
    s.push(challenge(true));
    await vi.waitFor(() => expect(s.sent.length).toBe(1));
    s.push({ re: 1, ok: welcome() });
    await p;
    expect(conn.isOnline).toBe(true);
    conn.close();
  });

  it("reports a ban as such, with the server's explanation", async () => {
    const conn = make();
    const p = conn.connect();
    const s = FakeSocket.last();
    s.open();
    s.push(challenge());
    await vi.waitFor(() => expect(s.sent.length).toBe(1));
    s.push({ re: 1, err: { code: "banned", message: "you are banned from this server: rude" } });
    await expect(p).rejects.toMatchObject({ kind: "banned", message: "you are banned from this server: rude" });
  });

  it("does not reconnect after a ban announced by the server", async () => {
    const conn = make();
    const s = await handshake(conn);
    s.push({ ev: "disconnected", d: { reason: { kind: "banned", by: "Admin", reason: "rude", until: null } } });
    s.drop();
    expect(conn.status).toMatchObject({ state: "closed", reason: { kind: "server", reason: { kind: "banned", reason: "rude" } } });
  });
});
