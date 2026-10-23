import "fake-indexeddb/auto";
import vectors from "../../../../crates/gwar-proto/tests/handshake-v4.json";
import { ed25519 } from "@noble/curves/ed25519.js";
import { encodeBase64Url } from "./base64url";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Connection, type ConnectionOptions } from "./connection";
import { generateIdentity } from "./identity";
import { decodeFixed, helloStatement, proofStatement, verifyProof } from "./handshake";
import { serverPins, checkCandidate, ServerIdentityChanged } from "./server-pins";
import { localState as pageState } from "../lib/local-state";
import { FakeSocket, fakeIdentity, testServerIdentity, challenge, finishHandshake, memoryPins } from "./testing";
import type { Hello } from "../proto/Hello";

beforeEach(() => { FakeSocket.reset(); });
function make(extra: Partial<ConnectionOptions> = {}) {
  return new Connection({ url: "wss://server.example/ws", identity: fakeIdentity, nickname: "test",
    client: { name: "test", version: "0", platform: "web" }, createSocket: (url) => new FakeSocket(url),
    pinStore: memoryPins(), ...extra });
}
async function begin(conn: Connection) {
  const result = conn.connect(); result.catch(() => {});
  const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
  await vi.waitFor(() => expect(socket.sent).toHaveLength(1));
  return { socket, result };
}

describe("gwar/4 pins and secrets", () => {
  it("pins first use only after proof, reuses a match and keeps first_seen", async () => {
    const pins = memoryPins();
    const first = make({ pinStore: pins, serverPassword: "secret" });
    const { socket, result } = await begin(first);
    expect(pins.pins.size).toBe(0);
    expect(socket.sent[0]!.d).not.toHaveProperty("server_password");
    expect(socket.sent[0]!.d).not.toHaveProperty("invite");
    await expect(first.request("ping", {})).rejects.toThrow();
    await finishHandshake(socket); await result;
    const pin = pins.load(first.endpoint)!;
    expect(pin.server_pk).toBe(first.serverKey);
    first.close();
    const second = make({ pinStore: pins });
    const next = await begin(second); await finishHandshake(next.socket); await next.result;
    expect(pins.load(second.endpoint)).toBe(pin);
    second.close();
  });

  it("blocks a pin mismatch before signing and sends nothing afterwards", async () => {
    const pins = memoryPins(); const other = await generateIdentity();
    pins.commit("wss://server.example/ws", other.publicKey);
    const sign = vi.fn(fakeIdentity.sign);
    const conn = make({ pinStore: pins, identity: { ...fakeIdentity, sign }, serverPassword: "secret", invite: "CODE" });
    const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
    await expect(result).rejects.toMatchObject({ kind: "server_identity", identityChange: { source: "pin" } });
    expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0); expect(socket.readyState).toBe(3);
    socket.push(challenge()); expect(socket.sent).toHaveLength(0);
    expect(pins.load(conn.endpoint)?.server_pk).toBe(other.publicKey);
  });

  it("invite fingerprint overrides first-contact TOFU before signing", async () => {
    const other = await generateIdentity(); const pins = memoryPins(); const sign = vi.fn(fakeIdentity.sign);
    const conn = make({ pinStore: pins, identity: { ...fakeIdentity, sign }, invite: "CODE", inviteServerKey: other.publicKey });
    const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
    await expect(result).rejects.toMatchObject({ kind: "server_identity", identityChange: { source: "invite" } });
    expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0); expect(pins.pins.size).toBe(0);
    const good = make({ pinStore: pins, invite: "CODE", inviteServerKey: testServerIdentity.publicKey });
    const next = await begin(good); await finishHandshake(next.socket); await next.result;
    expect(next.socket.sent[1]).toMatchObject({ op: "join", d: { invite: "CODE" } }); good.close();
  });

  it.each(["missing", "wrong", "changed transcript"])("%s proof is fatal: no pin, join, password or application command", async (mode) => {
    const pins = memoryPins(); const conn = make({ pinStore: pins, serverPassword: "secret", invite: "CODE" });
    const { socket, result } = await begin(conn);
    const hello = structuredClone(socket.sent[0]!.d) as Hello;
    let proof: unknown = {};
    if (mode !== "missing") {
      if (mode === "changed transcript") hello.endpoint = "wss://attacker.example/ws";
      const signer = mode === "wrong" ? await generateIdentity() : testServerIdentity;
      proof = { server_proof: await signer.sign(new TextEncoder().encode(await proofStatement(hello))) };
    }
    socket.push({ re: 1, ok: proof });
    await expect(result).rejects.toMatchObject({ kind: "protocol" });
    expect(socket.sent).toHaveLength(1); expect(pins.pins.size).toBe(0); expect(conn.serverKey).toBeNull();
    await expect(conn.request("ping", {})).rejects.toThrow();
  });

  it.each(["timeout", "closed"])("a %s before server proof is fatal", async (mode) => {
    const pins = memoryPins(); const conn = make({ pinStore: pins, handshakeTimeoutMs: 500, serverPassword: "secret" });
    const { socket, result } = await begin(conn);
    if (mode === "closed") socket.drop();
    await expect(result).rejects.toMatchObject({ kind: "protocol", message: "missing server proof" });
    expect(conn.status).toMatchObject({ state: "closed", reason: { kind: "error", error: { kind: "protocol" } } });
    expect(socket.sent).toHaveLength(1); expect(pins.pins.size).toBe(0);
  });

  it("rejects weak or invalid server keys before signing", async () => {
    for (const bytes of [new Uint8Array(32), Uint8Array.from([1, ...new Array(31).fill(0)]), new Uint8Array(32).fill(255)]) {
      const sign = vi.fn(fakeIdentity.sign); const conn = make({ identity: { ...fakeIdentity, sign } });
      const result = conn.connect(); const socket = FakeSocket.last(); socket.open();
      const frame = challenge(); frame.d.server_pk = encodeBase64Url(bytes); socket.push(frame);
      await expect(result).rejects.toMatchObject({ kind: "protocol" });
      expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
    }
  });

  it("rejects weak signature points independently of WebCrypto", async () => {
    const h = structuredClone(vectors[0]!.hello) as Hello;
    const identity = Uint8Array.from([1, ...new Array(31).fill(0)]);
    const signature = Uint8Array.from([...identity, ...new Array(32).fill(0)]);
    h.expected_server_pk = encodeBase64Url(identity);
    const acceptsEverything = vi.spyOn(crypto.subtle, "verify").mockResolvedValue(true);
    try {
      await expect(verifyProof(h, { server_proof: encodeBase64Url(signature) })).rejects.toThrow();
      h.expected_server_pk = testServerIdentity.publicKey;
      // A mathematically valid signature with R = identity is still forbidden by strict gwar/4.
      const secret = new Uint8Array(32).fill(3);
      h.expected_server_pk = encodeBase64Url(ed25519.getPublicKey(secret));
      const message = new TextEncoder().encode(await proofStatement(h));
      const input = Uint8Array.from([...identity, ...decodeFixed(h.expected_server_pk, 32), ...message]);
      const digest = new Uint8Array(await crypto.subtle.digest("SHA-512", input));
      const littleEndian = (bytes: Uint8Array) => bytes.reduceRight((n, byte) => (n << 8n) | BigInt(byte), 0n);
      let s = littleEndian(digest) * ed25519.utils.getExtendedPublicKey(secret).scalar % ed25519.Point.CURVE().n;
      for (let i = 32; i < 64; i++) { signature[i] = Number(s & 255n); s >>= 8n; }
      expect(ed25519.verify(signature, message, decodeFixed(h.expected_server_pk, 32), { zip215: false })).toBe(true);
      await expect(verifyProof(h, { server_proof: encodeBase64Url(signature) })).rejects.toThrow();
    } finally { acceptsEverything.mockRestore(); }
  });

  it("rejects a malformed challenge without signing or sending secrets", async () => {
    const sign = vi.fn(fakeIdentity.sign);
    const conn = make({ identity: { ...fakeIdentity, sign }, serverPassword: "secret" });
    const result = conn.connect(); const socket = FakeSocket.last(); socket.open();
    socket.push({ ev: "challenge", d: null });
    await expect(result).rejects.toMatchObject({ kind: "protocol" });
    expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
  });

  it("verifies proof before sending join or password", async () => {
    const conn = make({ serverPassword: "secret" });
    const { socket, result } = await begin(conn);
    expect(socket.sent.map((f) => f.op)).toEqual(["hello"]);
    const h = socket.sent[0]!.d as Hello;
    const server_proof = await testServerIdentity.sign(new TextEncoder().encode(await proofStatement(h)));
    socket.push({ re: 1, ok: { server_proof } });
    await vi.waitFor(() => expect(socket.sent[1]).toMatchObject({ op: "join", d: { server_password: "secret" } }));
    socket.push({ re: 2, ok: (await import("./testing")).welcome() }); await result; conn.close();
  });

  it("does not sign or commit a pin after the connection is closed during async crypto", async () => {
    const pins = memoryPins(); let resolve!: (value: string) => void;
    const sign = vi.fn(() => new Promise<string>((r) => { resolve = r; }));
    const conn = make({ pinStore: pins, identity: { ...fakeIdentity, sign } });
    const result = conn.connect(); result.catch(() => {}); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
    await vi.waitFor(() => expect(sign).toHaveBeenCalled()); conn.close();
    resolve("A".repeat(86)); await expect(result).rejects.toThrow();
    await new Promise((r) => setTimeout(r, 10)); expect(socket.sent).toHaveLength(0); expect(pins.pins.size).toBe(0);
  });
});

describe("gwar/4 transport", () => {
  it("blocks non-loopback plaintext without a desktop per-server toggle before opening a socket", async () => {
    for (const platform of ["web", "desktop"] as const) {
      const conn = make({ url: "ws://192.168.1.2:8790/ws", client: { name: "test", version: "0", platform } });
      await expect(conn.connect()).rejects.toMatchObject({ kind: "insecure_transport" });
      expect(FakeSocket.instances).toHaveLength(0);
    }
    await expect(make({ url: "ws://192.168.1.2/ws", insecureLan: true }).connect()).rejects.toMatchObject({ kind: "insecure_transport" });
  });
  it.each(["ws://127.0.0.1:8790/ws", "ws://localhost:8790/ws", "ws://[::1]:8790/ws"])("permits loopback %s", async (url) => {
    const conn = make({ url }); const { socket, result } = await begin(conn); await finishHandshake(socket); await result; conn.close();
  });
  it("permits a desktop LAN connection only with the per-server opt-in", async () => {
    const conn = make({ url: "ws://192.168.1.2:8790/ws", insecureLan: true, client: { name: "test", version: "0", platform: "desktop" } });
    const { socket, result } = await begin(conn); await finishHandshake(socket); await result; conn.close();
  });
});

describe("persistent pins and canonical fields", () => {
  it("persists canonical endpoint, key and first-seen time, and moves only to a key accepted in its place", async () => {
    const { localState } = await import("../lib/local-state");
    await localState.load();
    await serverPins.commit("wss://SERVER.example.:443/ws", testServerIdentity.publicKey);
    const pin = serverPins.load("wss://server.example/ws")!;
    expect(pin.server_pk).toBe(testServerIdentity.publicKey); expect(pin.first_seen).toBeGreaterThan(0);
    await serverPins.commit("wss://server.example/ws", testServerIdentity.publicKey);
    expect(serverPins.load("wss://server.example/ws")).toEqual(pin);
    expect(() => checkCandidate("wss://server.example/ws", fakeIdentity.publicKey, serverPins)).toThrow(ServerIdentityChanged);
    await expect(serverPins.commit("wss://server.example/ws", fakeIdentity.publicKey)).rejects.toBeInstanceOf(ServerIdentityChanged);
    // Accepted in place of another key than the pinned one: refused, the pin stays.
    await expect(serverPins.commit("wss://server.example/ws", fakeIdentity.publicKey, "B".repeat(42) + "A")).rejects.toBeInstanceOf(ServerIdentityChanged);
    expect(serverPins.load("wss://server.example/ws")).toEqual(pin);
    await serverPins.commit("wss://server.example/ws", fakeIdentity.publicKey, testServerIdentity.publicKey);
    expect(serverPins.load("wss://server.example/ws")?.server_pk).toBe(fakeIdentity.publicKey);
  });
  describe("accepting a changed server key", () => {
    const url = "wss://replace.example/ws";
    /** The key pinned before, the one the server now proves (`challenge` and `finishHandshake` use it), another. */
    const [old, proven] = [fakeIdentity.publicKey, testServerIdentity.publicKey];
    const pinned = () => serverPins.load(url)?.server_pk;
    async function accepted(extra: Partial<ConnectionOptions> = {}) {
      const localState = pageState;
      await localState.load();
      await localState.update("gwar.server-pin:" + url, () => JSON.stringify({ server_pk: old, first_seen: 1 }));
      return make({ url, pinStore: serverPins, inviteServerKey: proven, replaceServerKey: old, ...extra });
    }
    it("keeps the old pin until the new key is proven, so no third key is ever trusted on first use meanwhile", async () => {
      const other = (await generateIdentity()).publicKey;
      const conn = await accepted();
      const { socket, result } = await begin(conn);
      expect(pinned()).toBe(old);
      // Another tab that was waiting for a first-contact proof from this address finishes now: refused.
      await expect(serverPins.commit(url, other)).rejects.toBeInstanceOf(ServerIdentityChanged);
      await finishHandshake(socket); await result;
      expect(pinned()).toBe(proven);
      conn.close();
    });
    it("is refused when the pin no longer holds the key the user replaced", async () => {
      const other = (await generateIdentity()).publicKey;
      const conn = await accepted();
      const { socket, result } = await begin(conn);
      const localState = pageState;
      await localState.update("gwar.server-pin:" + url, () => JSON.stringify({ server_pk: other, first_seen: 2 }));
      const hello = socket.sent[0]!.d as Hello;
      socket.push({ re: 1, ok: { server_proof: await testServerIdentity.sign(new TextEncoder().encode(await proofStatement(hello))) } });
      await expect(result).rejects.toMatchObject({ kind: "server_identity" });
      expect(socket.sent).toHaveLength(1);
      expect(pinned()).toBe(other);
    });
    it("reads the stored pin before signing: a tab that missed another tab's replacement signs nothing (Sol r6 probe, inverted)", async () => {
      vi.resetModules();
      vi.stubGlobal("BroadcastChannel", undefined);
      try {
        const { createLocalState } = await import("../lib/local-state");
        const name = `stale-${crypto.randomUUID()}`;
        const [writer, stale] = [createLocalState(name), createLocalState(name)];
        await writer.load(); await stale.load();
        const pinsOf = async (state: typeof writer) => {
          vi.resetModules();
          vi.doMock("../lib/local-state", async () => ({ ...await vi.importActual("../lib/local-state"), localState: state }));
          return (await import("./server-pins")).serverPins;
        };
        const [mine, other] = [await pinsOf(stale), await pinsOf(writer)];
        await other.commit(url, proven);
        await stale.refresh();
        // The other tab's user accepts a new key; this tab is not told.
        await other.commit(url, old, proven);
        expect(mine.load(url)?.server_pk).toBe(proven);
        const { Connection: Isolated } = await import("./connection");
        const sign = vi.fn(fakeIdentity.sign);
        const conn = new Isolated({ url, identity: { ...fakeIdentity, sign }, nickname: "test", pinStore: mine,
          client: { name: "test", version: "0", platform: "web" }, createSocket: (u) => new FakeSocket(u) });
        const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
        await expect(result).rejects.toMatchObject({ kind: "server_identity", identityChange: { expected: old, candidate: proven } });
        expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
        writer.close(); stale.close();
      } finally { vi.doUnmock("../lib/local-state"); vi.unstubAllGlobals(); vi.resetModules(); }
    });
    it("needs the accepted key: without it the old pin blocks before signing", async () => {
      const sign = vi.fn(fakeIdentity.sign);
      const conn = await accepted({ inviteServerKey: undefined, identity: { ...fakeIdentity, sign } });
      const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
      await expect(result).rejects.toMatchObject({ kind: "server_identity" });
      expect(sign).not.toHaveBeenCalled(); expect(pinned()).toBe(old);
    });
  });
  it("trusts no key while the pins cannot be read: nothing pinned or signed, and a storage error", async () => {
    vi.resetModules();
    vi.stubGlobal("indexedDB", undefined);
    try {
      const { localState } = await import("../lib/local-state");
      await localState.load();
      const { serverPins: pins } = await import("./server-pins");
      expect(() => pins.load("wss://server.example/ws")).toThrow(/no new key is trusted/);
      await expect(pins.commit("wss://server.example/ws", testServerIdentity.publicKey)).rejects.toMatchObject({ name: "LocalStateUnavailableError" });
      const { Connection: Isolated } = await import("./connection");
      const sign = vi.fn(fakeIdentity.sign);
      const conn = new Isolated({ url: "wss://server.example/ws", identity: { ...fakeIdentity, sign }, nickname: "test",
        client: { name: "test", version: "0", platform: "web" }, createSocket: (url) => new FakeSocket(url), pinStore: pins });
      const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
      await expect(result).rejects.toMatchObject({ kind: "trust_storage" });
      expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
    } finally { vi.unstubAllGlobals(); vi.resetModules(); }
  });
  it("loads the security state itself on a page that did not load it at start (the e2e fixtures)", async () => {
    vi.resetModules();
    try {
      // Fresh modules, as on a page whose entry is not main.tsx: nothing has called load().
      const { Connection: Isolated } = await import("./connection");
      const { serverPins: pins } = await import("./server-pins");
      const url = `wss://fixture-${crypto.randomUUID()}.example/ws`;
      const conn = new Isolated({ url, identity: fakeIdentity, nickname: "test",
        client: { name: "test", version: "0", platform: "web" }, createSocket: (u) => new FakeSocket(u) });
      const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
      await vi.waitFor(() => expect(socket.sent).toHaveLength(1));
      await finishHandshake(socket); await result;
      expect(pins.load(url)?.server_pk).toBe(testServerIdentity.publicKey);
      conn.close();
    } finally { vi.resetModules(); }
  });
  it("trusts no key once the browser closed the pins' database (Sol r5 probe, inverted)", async () => {
    vi.resetModules();
    try {
      let db!: IDBDatabase;
      const { createLocalState, LOCAL_STATE_MIGRATIONS } = await import("../lib/local-state");
      const state = createLocalState(`closed-${crypto.randomUUID()}`, [(opened, tx) => { db = opened; LOCAL_STATE_MIGRATIONS[0]!(opened, tx); }]);
      await state.load();
      vi.doMock("../lib/local-state", async () => ({ ...await vi.importActual("../lib/local-state"), localState: state }));
      const { Connection: Isolated } = await import("./connection");
      const { forceCloseDatabase } = await import("fake-indexeddb");
      forceCloseDatabase(db as never);
      await vi.waitFor(() => expect(state.error()).toBeDefined());
      const sign = vi.fn(fakeIdentity.sign);
      const conn = new Isolated({ url: "wss://closed.example/ws", identity: { ...fakeIdentity, sign }, nickname: "test",
        client: { name: "test", version: "0", platform: "web" }, createSocket: (url) => new FakeSocket(url) });
      const result = conn.connect(); const socket = FakeSocket.last(); socket.open(); socket.push(challenge());
      await expect(result).rejects.toMatchObject({ kind: "trust_storage" });
      expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
    } finally { vi.doUnmock("../lib/local-state"); vi.resetModules(); }
  });
  it.each(["A".repeat(43) + "=", "A".repeat(42) + "B", "bad\nkey", "AA"])("rejects noncanonical encodings: %s", (value) => expect(() => decodeFixed(value, 32)).toThrow());
  it("has exact statement bytes with no trailing newline", () => {
    const key = "A".repeat(43); const h = { endpoint: "wss://server.example/ws", expected_server_pk: key, server_nonce: key, client_nonce: key, client_signing_pk: key, certificate_hash: "-" };
    expect(helloStatement(h)).toBe(`gwar client hello v4\nwss://server.example/ws\n${key}\n${key}\n${key}\n${key}\n-`);
  });
});

describe("shared gwar/4 wire vectors", () => {
  it.each(vectors)("matches Rust H, certificate commitment, hello hash and proof", async (v) => {
    const h = v.hello as Hello;
    expect(helloStatement(h)).toBe(v.statement);
    expect(await (await import("./handshake")).hashStatement(v.statement)).toBe(v.hello_hash);
    expect(await proofStatement(h)).toBe(v.proof_statement);
    await expect(verifyProof(h, { server_proof: v.server_proof })).resolves.toBeUndefined();
    const key = await crypto.subtle.importKey("raw", decodeFixed(h.client_signing_pk, 32) as BufferSource, "Ed25519", false, ["verify"]);
    expect(await crypto.subtle.verify("Ed25519", key, decodeFixed(h.signature, 64) as BufferSource, new TextEncoder().encode(v.statement))).toBe(true);
  });
});

describe("gwar/4 immutable endpoint and runtime validation", () => {
  it("stores the canonical endpoint once for dialing, transcript and pinning", async () => {
    const options: Partial<ConnectionOptions> = { url: "wss://SERVER.example.:443/ws" };
    const conn = make(options);
    expect(conn.endpoint).toBe("wss://server.example/ws");
    // Mutating the caller's options cannot change the authenticated endpoint.
    const mutable = { url: "wss://SERVER.example.:443/ws", identity: fakeIdentity, nickname: "test",
      client: { name: "test", version: "0", platform: "web" as const }, createSocket: (url: string) => new FakeSocket(url), pinStore: memoryPins() };
    const cached = new Connection(mutable);
    mutable.url = "invalid";
    const { socket, result } = await begin(cached);
    expect(socket.url).toBe(cached.endpoint);
    expect(socket.sent[0]!.d).toMatchObject({ endpoint: cached.endpoint });
    await finishHandshake(socket); await result;
    expect(cached.serverKey).toBe(testServerIdentity.publicKey);
    cached.close();
  });
  it("rejects a challenge failing its schema before signing or sending secrets", async () => {
    const sign = vi.fn(fakeIdentity.sign);
    const conn = make({ identity: { ...fakeIdentity, sign }, serverPassword: "secret" });
    const result = conn.connect(); const socket = FakeSocket.last(); socket.open();
    const valid = challenge(); socket.push({ ...valid, d: { ...valid.d, extra: "hostile" } });
    await expect(result).rejects.toMatchObject({ kind: "protocol" });
    expect(sign).not.toHaveBeenCalled(); expect(socket.sent).toHaveLength(0);
  });
  it("rejects a proof failing its schema without pinning or joining", async () => {
    const pins = memoryPins(); const conn = make({ pinStore: pins, serverPassword: "secret" });
    const { socket, result } = await begin(conn);
    socket.push({ re: 1, ok: { server_proof: "A".repeat(86), extra: "hostile" } });
    await expect(result).rejects.toMatchObject({ kind: "protocol" });
    expect(pins.pins.size).toBe(0); expect(socket.sent).toHaveLength(1);
  });
});
