/**
 * A session on a TeamSpeak 3/6 server (desktop app only). The native client
 * presents the TS server through `vc/1` shapes (see `vc_client::teamspeak`), so
 * this class is the Tauri-bridge twin of `Connection`: same `Link` surface,
 * same Welcome / events / requests, no WebSocket and no vc handshake.
 */
import type { Event } from "../proto/Event";
import type { ErrorCode } from "../proto/ErrorCode";
import type { LeaveReason } from "../proto/LeaveReason";
import type { Welcome } from "../proto/Welcome";
import { tauri, type TauriBridge, type Unlisten } from "../platform";
import {
  backoffDelay,
  ConnectError,
  ConnectionLostError,
  RequestError,
  RequestTimeoutError,
  shouldReconnectAfter,
  type ConnectionStatus,
  type Link,
} from "./connection";
import { parseFrame } from "./frames";
import type { EventData, EventName, Op, RequestInput, ResponseMap } from "./protocol";

export interface TsConnectionOptions {
  /** `host:port`, already normalised (see `parseTeamSpeakAddress`). */
  address: string;
  nickname: string;
  serverPassword?: string;
  requestTimeoutMs?: number;
  /** Test seams. */
  bridge?: () => Promise<TauriBridge>;
  random?: () => number;
}

type Handler = (data: never) => void;

let sessionCounter = 0;

/** Native connect failures arrive as plain strings; classify the ones the UI explains. */
export function classifyTsError(message: string, hadPassword: boolean, serverName?: string): ConnectError {
  const m = message.toLowerCase();
  if (/password/.test(m)) return new ConnectError(hadPassword ? "wrong_password" : "password_required", message, serverName);
  if (/(banned|ban )/.test(m)) return new ConnectError("rejected", message, serverName);
  if (/(max.?clients|server is full|full)/.test(m)) return new ConnectError("server_full", message, serverName);
  if (/(timed out|timeout)/.test(m)) return new ConnectError("timeout", message, serverName);
  return new ConnectError("unreachable", message, serverName);
}

function isErrorBody(e: unknown): e is { code: ErrorCode; message: string } {
  return typeof e === "object" && e !== null && "code" in e && "message" in e;
}

export class TsConnection implements Link {
  private status_: ConnectionStatus = { state: "idle" };
  private api: TauriBridge | null = null;
  private unlisten: Unlisten[] = [];
  private session = 0;
  private eventHandlers = new Map<string, Set<Handler>>();
  private anyHandlers = new Set<(event: Event) => void>();
  private welcomeHandlers = new Set<(welcome: Welcome, resync: boolean) => void>();
  private statusHandlers = new Set<(status: ConnectionStatus) => void>();
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private attempt = 0;
  private intentionalClose = false;
  private serverReason: LeaveReason | null = null;
  private hasBeenOnline = false;
  /** Events that arrive before the Welcome has been handed out wait here. */
  private held: Event[] | null = null;

  constructor(private readonly options: TsConnectionOptions) {}

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

  onEvent(handler: (event: Event) => void): () => void {
    this.anyHandlers.add(handler);
    return () => this.anyHandlers.delete(handler);
  }

  async connect(): Promise<Welcome> {
    if (this.status_.state !== "idle") throw new Error("connect() may only be called once");
    this.setStatus({ state: "connecting" });
    try {
      const welcome = await this.open();
      this.hasBeenOnline = true;
      this.setStatus({ state: "online" });
      this.deliverWelcome(welcome, false);
      return welcome;
    } catch (e) {
      const error = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      if (!this.intentionalClose) this.setStatus({ state: "closed", reason: { kind: "error", error } });
      throw error;
    }
  }

  async request<O extends Op>(op: O, d: RequestInput<O>, opts: { timeoutMs?: number } = {}): Promise<ResponseMap[O]> {
    const api = this.api;
    if (!api || this.status_.state !== "online") throw new ConnectionLostError();
    const timeoutMs = opts.timeoutMs ?? this.options.requestTimeoutMs ?? 20_000;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const timeout = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new RequestTimeoutError(op)), timeoutMs);
    });
    try {
      return (await Promise.race([api.invoke<unknown>("ts_request", { session: this.session, op, d }), timeout])) as ResponseMap[O];
    } catch (e) {
      if (e instanceof RequestTimeoutError) throw e;
      if (isErrorBody(e)) throw new RequestError(e.code, e.message);
      throw new RequestError("internal", e instanceof Error ? e.message : String(e));
    } finally {
      clearTimeout(timer);
    }
  }

  retryNow(): void {
    if (this.status_.state !== "reconnecting") return;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    void this.reconnectAttempt();
  }

  close(): void {
    this.intentionalClose = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    const session = this.session;
    this.session = 0;
    for (const fn of this.unlisten.splice(0)) fn();
    if (session && this.api) void this.api.invoke("ts_disconnect", { session }).catch(() => {});
    if (this.status_.state !== "closed") this.setStatus({ state: "closed", reason: { kind: "user" } });
  }

  // ------------------------------------------------------------------ internals

  private setStatus(status: ConnectionStatus): void {
    this.status_ = status;
    for (const h of [...this.statusHandlers]) h(status);
  }

  /** Hands out the Welcome, then the events that raced it. */
  private deliverWelcome(welcome: Welcome, resync: boolean): void {
    for (const h of [...this.welcomeHandlers]) h(welcome, resync);
    const held = this.held ?? [];
    this.held = null;
    for (const event of held) this.route(event);
  }

  private async ensureListeners(api: TauriBridge): Promise<void> {
    if (this.unlisten.length > 0) return;
    this.unlisten.push(
      await api.listen<{ session: number; frame: string }>("ts://event", (p) => {
        if (p.session !== this.session) return;
        const frame = parseFrame(p.frame);
        if (frame?.kind !== "event") return;
        if (this.held) this.held.push(frame.event);
        else this.route(frame.event);
      }),
      await api.listen<{ session: number }>("ts://closed", (p) => {
        if (p.session === this.session) this.onClosed();
      }),
    );
  }

  private route(event: Event): void {
    if (event.ev === "disconnected") this.serverReason = event.d.reason;
    const set = this.eventHandlers.get(event.ev);
    if (set) for (const h of [...set]) h(event.d as never);
    for (const h of [...this.anyHandlers]) h(event);
  }

  private async open(): Promise<Welcome> {
    const api = (this.api ??= await (this.options.bridge ?? tauri)());
    await this.ensureListeners(api);
    if (this.intentionalClose) throw new ConnectError("unreachable", "closed");
    const session = (this.session = ++sessionCounter);
    this.serverReason = null;
    this.held = [];
    try {
      return await api.invoke<Welcome>("ts_connect", {
        session,
        address: this.options.address,
        nickname: this.options.nickname,
        password: this.options.serverPassword ?? null,
      });
    } catch (e) {
      this.held = null;
      if (this.session === session) this.session = 0;
      throw classifyTsError(typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e), !!this.options.serverPassword, this.options.address);
    }
  }

  private onClosed(): void {
    this.session = 0;
    if (this.intentionalClose || this.status_.state === "closed") return;
    const reason = this.serverReason;
    if (reason && !shouldReconnectAfter(reason)) {
      this.setStatus({ state: "closed", reason: { kind: "server", reason } });
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
      const welcome = await this.open();
      if (this.intentionalClose) return;
      this.attempt = 0;
      this.setStatus({ state: "online" });
      this.deliverWelcome(welcome, this.hasBeenOnline);
    } catch (e) {
      if (this.intentionalClose) return;
      const error = e instanceof ConnectError ? e : new ConnectError("unreachable", String(e));
      if (error.kind === "wrong_password" || error.kind === "password_required" || error.kind === "rejected") {
        this.setStatus({ state: "closed", reason: { kind: "error", error } });
        return;
      }
      this.scheduleReconnect(attempt + 1);
    }
  }
}
