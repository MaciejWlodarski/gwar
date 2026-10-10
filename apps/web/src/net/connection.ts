import type { BanNotice } from "../proto/BanNotice";
import type { ErrorCode } from "../proto/ErrorCode";
import type { LeaveReason } from "../proto/LeaveReason";
import type { Welcome } from "../proto/Welcome";
import type { Challenge } from "../proto/Challenge";
import type { Event } from "../proto/Event";
import { encodeRequest, parseFrame, type ParsedFrame } from "./frames";
import { challengeMessage, type Identity } from "./identity";
import {
  PROTOCOL_VERSION,
  type EventData,
  type EventName,
  type Op,
  type RequestInput,
  type ResponseMap,
} from "./protocol";

/** Subset of `WebSocket` the connection needs; lets tests inject a fake. */
export interface WebSocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(code?: number, reason?: string): void;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number; reason: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
}

export type ConnectErrorKind =
  | "unreachable" // socket never opened / dropped before the challenge
  | "timeout" // server did not answer the handshake in time
  | "password_required" // challenge says a server password is needed and none was given
  | "wrong_password"
  | "server_full"
  | "banned" // the server refuses this user or address; the message says why and until when
  | "certificate_expired" // the Gwar Connect device certificate we sent has run out; renew it in Settings > Account
  | "rejected" // any other error reply to hello (bad nickname, ...)
  | "protocol"; // protocol version mismatch or malformed handshake

export class ConnectError extends Error {
  constructor(
    readonly kind: ConnectErrorKind,
    message: string,
    readonly serverName?: string,
    /** Who banned us, why and until when (with `banned`). */
    readonly ban?: BanNotice,
  ) {
    super(message);
    this.name = "ConnectError";
  }
}

export class RequestError extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
    readonly ban?: BanNotice,
  ) {
    super(message);
    this.name = "RequestError";
  }
}

export class RequestTimeoutError extends Error {
  constructor(readonly op: string) {
    super(`request ${op} timed out`);
    this.name = "RequestTimeoutError";
  }
}

export class ConnectionLostError extends Error {
  constructor() {
    super("connection lost");
    this.name = "ConnectionLostError";
  }
}

export type CloseReason =
  | { kind: "user" }
  | { kind: "server"; reason: LeaveReason }
  | { kind: "error"; error: ConnectError };

export type ConnectionStatus =
  | { state: "idle" }
  | { state: "connecting" }
  | { state: "online" }
  | { state: "reconnecting"; attempt: number; retryAt: number }
  | { state: "closed"; reason: CloseReason };

export interface ClientInfo {
  name: string;
  version: string;
  platform: "web" | "desktop" | "mobile";
}

export interface ConnectionOptions {
  url: string;
  identity: Identity;
  nickname: string;
  serverPassword?: string;
  /** Invite code: admits without the server password. Used for the first connect only. */
  invite?: string;
  client: ClientInfo;
  /** Test seams. */
  createSocket?: (url: string) => WebSocketLike;
  random?: () => number;
  handshakeTimeoutMs?: number;
  requestTimeoutMs?: number;
  pingIntervalMs?: number;
  /** Set false to surface drops as `closed` instead of retrying. */
  autoReconnect?: boolean;
}

const OPEN = 1;

/** Exponential backoff with jitter: ~0.5s, 1s, 2s ... capped at 15s. */
export function backoffDelay(attempt: number, random: () => number = Math.random): number {
  const base = Math.min(500 * 2 ** Math.max(0, attempt - 1), 15_000);
  return Math.round(base * (0.75 + random() * 0.5));
}

/** Whether a server-announced departure should be followed by a reconnect. */
export function shouldReconnectAfter(reason: LeaveReason): boolean {
  return reason.kind !== "kicked" && reason.kind !== "banned" && reason.kind !== "removed" && reason.kind !== "replaced";
}

/**
 * Servers answer an unusable device certificate with the generic `not_authenticated` (the same code
 * as a bad signature or a revoked device), so the cause is told apart here: our own clock against the
 * certificate we sent, or the server's wording ("the device certificate has expired").
 */
export function isExpiredCertificate(device: Identity["device"], message: string, now: number = Date.now()): boolean {
  return !!device && (device.expires_at <= now || /certificate has expired/i.test(message));
}

function mapHelloError(code: ErrorCode, message: string, serverName?: string, ban?: BanNotice, device?: Identity["device"]): ConnectError {
  switch (code) {
    case "certificate_expired":
      return new ConnectError("certificate_expired", message, serverName);
    case "not_authenticated":
      // Servers before the certificate_expired code only said so in the message.
      return new ConnectError(isExpiredCertificate(device, message) ? "certificate_expired" : "rejected", message, serverName);
    case "wrong_password":
      return new ConnectError("wrong_password", message, serverName);
    case "unavailable":
      return new ConnectError("server_full", message, serverName);
    case "banned":
      return new ConnectError("banned", message, serverName, ban);
    default:
      return new ConnectError("rejected", message, serverName);
  }
}

/** Errors after which retrying cannot help. */
function isFatal(e: ConnectError): boolean {
  return (
    e.kind === "wrong_password" ||
    e.kind === "password_required" ||
    e.kind === "protocol" ||
    e.kind === "rejected" ||
    e.kind === "certificate_expired" ||
    e.kind === "banned"
  );
}

/**
 * What the controller needs from a server session, whatever the transport:
 * `Connection` (WebSocket, vc/1) or `TsConnection` (Tauri bridge to a
 * TeamSpeak server, see ts-connection.ts). Both speak vc/1 shapes.
 */
export interface Link {
  readonly status: ConnectionStatus;
  readonly isOnline: boolean;
  onStatus(handler: (status: ConnectionStatus) => void): () => void;
  onWelcome(handler: (welcome: Welcome, resync: boolean) => void): () => void;
  on<E extends EventName>(ev: E, handler: (data: EventData<E>) => void): () => void;
  onEvent(handler: (event: Event) => void): () => void;
  connect(): Promise<Welcome>;
  request<O extends Op>(op: O, d: RequestInput<O>, opts?: { timeoutMs?: number }): Promise<ResponseMap[O]>;
  retryNow(): void;
  close(): void;
}

interface Pending {
  op: string;
  resolve: (value: never) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

type Handler = (data: never) => void;

/**
 * One logical vc/1 session: handshake, request correlation, typed events and
 * automatic reconnection. Every (re)connect ends with a fresh Welcome, which
 * `onWelcome` delivers so consumers can replace all derived state.
 */
export class Connection implements Link {
  private status_: ConnectionStatus = { state: "idle" };
  private socket: WebSocketLike | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private eventHandlers = new Map<string, Set<Handler>>();
  private anyHandlers = new Set<(event: Event) => void>();
  private welcomeHandlers = new Set<(welcome: Welcome, resync: boolean) => void>();
  private statusHandlers = new Set<(status: ConnectionStatus) => void>();
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private attempt = 0;
  private intentionalClose = false;
  private serverReason: LeaveReason | null = null;
  private hasBeenOnline = false;
  private generation = 0;
  private abortHandshake: (() => void) | null = null;
  /** Spent after the first welcome: a reconnect is the same member, and the invite may be used up. */
  private invite: string | undefined;

  constructor(private readonly options: ConnectionOptions) {
    this.invite = options.invite;
  }

  get status(): ConnectionStatus {
    return this.status_;
  }

  get isOnline(): boolean {
    return this.status_.state === "online";
  }

  onStatus(handler: (status: ConnectionStatus) => void): () => void {
    this.statusHandlers.add(handler);
    return () => this.statusHandlers.delete(handler);
  }

  onWelcome(handler: (welcome: Welcome, resync: boolean) => void): () => void {
    this.welcomeHandlers.add(handler);
    return () => this.welcomeHandlers.delete(handler);
  }

  on<E extends EventName>(ev: E, handler: (data: EventData<E>) => void): () => void {
    let set = this.eventHandlers.get(ev);
    if (!set) this.eventHandlers.set(ev, (set = new Set()));
    set.add(handler as Handler);
    return () => set.delete(handler as Handler);
  }

  /** Receives every server event (after the typed subscribers). */
  onEvent(handler: (event: Event) => void): () => void {
    this.anyHandlers.add(handler);
    return () => this.anyHandlers.delete(handler);
  }

  /** First connection. Rejects with {@link ConnectError}; never retries by itself. */
  async connect(): Promise<Welcome> {
    if (this.status_.state !== "idle") throw new Error("connect() may only be called once");
    this.setStatus({ state: "connecting" });
    try {
      const welcome = await this.openAndHandshake();
      this.invite = undefined;
      this.hasBeenOnline = true;
      this.setStatus({ state: "online" });
      this.emitWelcome(welcome, false);
      return welcome;
    } catch (e) {
      const error = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      if (!this.intentionalClose) this.setStatus({ state: "closed", reason: { kind: "error", error } });
      throw error;
    }
  }

  request<O extends Op>(op: O, d: RequestInput<O>, opts: { timeoutMs?: number } = {}): Promise<ResponseMap[O]> {
    const socket = this.socket;
    if (!socket || socket.readyState !== OPEN || this.status_.state !== "online") {
      return Promise.reject(new ConnectionLostError());
    }
    return this.send(socket, op, d as Record<string, unknown>, opts.timeoutMs ?? this.options.requestTimeoutMs ?? 10_000);
  }

  /** Skips the backoff wait (e.g. when the browser reports it is back online). */
  retryNow(): void {
    if (this.status_.state !== "reconnecting") return;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    void this.reconnectAttempt();
  }

  close(): void {
    this.intentionalClose = true;
    this.generation++;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.stopPing();
    this.abortHandshake?.();
    this.failPending(new ConnectionLostError());
    const socket = this.socket;
    this.socket = null;
    if (socket) {
      detach(socket);
      socket.close(1000, "bye");
    }
    if (this.status_.state !== "closed") this.setStatus({ state: "closed", reason: { kind: "user" } });
  }

  // ------------------------------------------------------------------ internals

  private setStatus(status: ConnectionStatus): void {
    this.status_ = status;
    for (const h of [...this.statusHandlers]) h(status);
  }

  private emitWelcome(welcome: Welcome, resync: boolean): void {
    for (const h of [...this.welcomeHandlers]) h(welcome, resync);
  }

  private send<O extends Op>(
    socket: WebSocketLike,
    op: O,
    d: Record<string, unknown>,
    timeoutMs: number,
    forcedId?: number,
  ): Promise<ResponseMap[O]> {
    const id = forcedId ?? this.nextId++;
    return new Promise<ResponseMap[O]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new RequestTimeoutError(op));
      }, timeoutMs);
      this.pending.set(id, { op, resolve: resolve as (v: never) => void, reject, timer });
      try {
        socket.send(encodeRequest(id, op, d));
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    });
  }

  private failPending(error: Error): void {
    const all = [...this.pending.values()];
    this.pending.clear();
    for (const p of all) {
      clearTimeout(p.timer);
      p.reject(error);
    }
  }

  /** Opens a socket, runs challenge/hello, and leaves the socket wired for normal traffic. */
  private openAndHandshake(): Promise<Welcome> {
    const { url, identity, nickname, serverPassword, client } = this.options;
    const create = this.options.createSocket ?? ((u: string) => new WebSocket(u) as unknown as WebSocketLike);
    const timeoutMs = this.options.handshakeTimeoutMs ?? 10_000;
    const generation = ++this.generation;
    this.nextId = 1;
    this.serverReason = null;

    return new Promise<Welcome>((resolve, reject) => {
      let socket: WebSocketLike;
      try {
        socket = create(url);
      } catch (e) {
        reject(new ConnectError("unreachable", e instanceof Error ? e.message : String(e)));
        return;
      }
      let settled = false;
      let serverName: string | undefined;
      const fail = (error: ConnectError) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        this.abortHandshake = null;
        detach(socket);
        try {
          socket.close();
        } catch {
          /* already closed */
        }
        if (this.socket === socket) this.socket = null;
        this.failPending(new ConnectionLostError());
        reject(error);
      };
      this.abortHandshake = () => fail(new ConnectError("unreachable", "closed"));
      const timer = setTimeout(
        () => fail(new ConnectError("timeout", "server did not complete the handshake", serverName)),
        timeoutMs,
      );
      this.socket = socket;
      let gotChallenge = false;

      socket.onerror = () => {
        /* followed by onclose; reported there */
      };
      socket.onclose = () => {
        fail(new ConnectError("unreachable", "connection closed during handshake", serverName));
      };
      socket.onmessage = (msg) => {
        if (settled || generation !== this.generation || typeof msg.data !== "string") return;
        const frame = parseFrame(msg.data);
        if (!frame) return;
        if (!gotChallenge) {
          if (frame.kind !== "event" || frame.event.ev !== "challenge") {
            fail(new ConnectError("protocol", "expected challenge"));
            return;
          }
          gotChallenge = true;
          const challenge: Challenge = frame.event.d;
          serverName = challenge.server.name;
          if (challenge.protocol !== PROTOCOL_VERSION) {
            fail(new ConnectError("protocol", `server speaks protocol ${challenge.protocol}`, serverName));
            return;
          }
          if (challenge.password_required && !serverPassword && !this.invite) {
            fail(new ConnectError("password_required", "server password required", serverName));
            return;
          }
          void this.sendHello(socket, challenge, nickname, serverPassword, identity, client, timeoutMs)
            .then((welcome) => {
              if (settled) return;
              settled = true;
              clearTimeout(timer);
              this.abortHandshake = null;
              this.wireSocket(socket, generation);
              resolve(welcome);
            })
            .catch((e: unknown) => {
              if (e instanceof RequestError) fail(mapHelloError(e.code, e.message, serverName, e.ban, identity.device));
              else if (e instanceof RequestTimeoutError) fail(new ConnectError("timeout", e.message, serverName));
              else fail(new ConnectError("protocol", e instanceof Error ? e.message : String(e), serverName));
            });
          return;
        }
        this.routeFrame(frame);
      };
    });
  }

  private async sendHello(
    socket: WebSocketLike,
    challenge: Challenge,
    nickname: string,
    serverPassword: string | undefined,
    identity: Identity,
    client: ClientInfo,
    timeoutMs: number,
  ): Promise<Welcome> {
    const signature = await identity.sign(challengeMessage(challenge.nonce, identity.publicKey));
    const hello: Record<string, unknown> = {
      protocol: PROTOCOL_VERSION,
      nickname,
      public_key: identity.publicKey,
      signature,
      client,
    };
    if (identity.device) hello.device = identity.device;
    if (serverPassword) hello.server_password = serverPassword;
    if (this.invite) hello.invite = this.invite;
    return this.send(socket, "hello", hello, timeoutMs, 1);
  }

  private wireSocket(socket: WebSocketLike, generation: number): void {
    this.nextId = 2;
    socket.onmessage = (msg) => {
      if (generation !== this.generation || typeof msg.data !== "string") return;
      const frame = parseFrame(msg.data);
      if (frame) this.routeFrame(frame);
    };
    socket.onclose = () => {
      if (generation !== this.generation) return;
      this.onSocketClosed();
    };
    socket.onerror = () => {};
    this.startPing();
  }

  private routeFrame(frame: ParsedFrame): void {
    if (frame.kind === "ok" || frame.kind === "err") {
      const p = this.pending.get(frame.re);
      if (!p) return;
      this.pending.delete(frame.re);
      clearTimeout(p.timer);
      if (frame.kind === "ok") p.resolve(frame.ok as never);
      else p.reject(new RequestError(frame.err.code, frame.err.message, frame.err.ban ?? undefined));
      return;
    }
    const { event } = frame;
    if (event.ev === "disconnected") this.serverReason = event.d.reason;
    const set = this.eventHandlers.get(event.ev);
    if (set) for (const h of [...set]) h(event.d as never);
    for (const h of [...this.anyHandlers]) h(event);
  }

  private onSocketClosed(): void {
    this.stopPing();
    this.socket = null;
    this.failPending(new ConnectionLostError());
    if (this.intentionalClose || this.status_.state === "closed") return;

    const reason = this.serverReason;
    if (reason && !shouldReconnectAfter(reason)) {
      this.setStatus({ state: "closed", reason: { kind: "server", reason } });
      return;
    }
    if (this.options.autoReconnect === false) {
      this.setStatus({
        state: "closed",
        reason: reason ? { kind: "server", reason } : { kind: "error", error: new ConnectError("unreachable", "connection lost") },
      });
      return;
    }
    this.scheduleReconnect(1);
  }

  private scheduleReconnect(attempt: number): void {
    this.attempt = attempt;
    const delay = backoffDelay(attempt, this.options.random);
    this.setStatus({ state: "reconnecting", attempt, retryAt: Date.now() + delay });
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      void this.reconnectAttempt();
    }, delay);
  }

  private async reconnectAttempt(): Promise<void> {
    if (this.intentionalClose || this.status_.state !== "reconnecting") return;
    const attempt = this.attempt;
    try {
      const welcome = await this.openAndHandshake();
      if (this.intentionalClose) return;
      this.attempt = 0;
      this.setStatus({ state: "online" });
      this.emitWelcome(welcome, this.hasBeenOnline);
    } catch (e) {
      if (this.intentionalClose) return;
      const error = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      if (isFatal(error)) {
        this.setStatus({ state: "closed", reason: { kind: "error", error } });
        return;
      }
      this.scheduleReconnect(attempt + 1);
    }
  }

  private startPing(): void {
    this.stopPing();
    const interval = this.options.pingIntervalMs ?? 20_000;
    this.pingTimer = setInterval(() => {
      const socket = this.socket;
      if (!socket) return;
      this.request("ping", {}, { timeoutMs: 10_000 }).catch((e: unknown) => {
        // No pong in time: treat the link as dead so reconnect kicks in.
        if (e instanceof RequestTimeoutError && this.socket === socket) {
          detach(socket);
          socket.close();
          this.onSocketClosed();
        }
      });
    }, interval);
  }

  private stopPing(): void {
    if (this.pingTimer) clearInterval(this.pingTimer);
    this.pingTimer = null;
  }
}

function detach(socket: WebSocketLike): void {
  socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
}
