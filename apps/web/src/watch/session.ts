/**
 * A TeamSpeak guest's watch session (docs/screen-share.md, "TeamSpeak watch links"): one
 * WebSocket to `/ws/watch`, `guest.watch {token}`, then a one-slot receive-only watch transport
 * offered with `guest.offer`, and `ping {}` every 20 s so the server's idle timeout never closes
 * a guest that is still watching. No identity, no MLS and nothing stored on the device.
 */
import { parseServerAddress, sameOriginUrl, type PageContext } from "../net/address";
import type { IceServer } from "../proto/IceServer";
import type { TransportState, WatchParams } from "../voice/screen";

export type WatchStatus =
  | "connecting" | "watching" | "slow" | "paused"
  /** Final: nothing more happens on this page. */
  | "ended" | "off" | "invalid" | "busy" | "lost" | "unsupported" | "codec";

const FINAL: ReadonlySet<WatchStatus> = new Set(["ended", "off", "invalid", "busy", "lost", "unsupported", "codec"]);

/** The guest's ping interval, as `net/connection.ts` pings members. */
export const PING_INTERVAL_MS = 20_000;

/** The guest endpoint: `?server=` (as in invite links) when the page has one, else this page's own server. */
export function watchEndpoint(search: string, page: PageContext): string | null {
  const server = new URLSearchParams(search).get("server")?.trim();
  let url: string;
  if (server) {
    const parsed = parseServerAddress(server, page);
    if (!parsed.ok) return null;
    url = parsed.value.url;
  } else {
    url = sameOriginUrl(page);
  }
  const endpoint = new URL(url);
  endpoint.pathname = "/ws/watch";
  return endpoint.toString();
}

/** The token in the fragment: 32 bytes in base64url, nothing else. */
export function readToken(hash: string): string | null {
  const token = hash.replace(/^#/, "");
  return /^[A-Za-z0-9_-]{43}$/.test(token) ? token : null;
}

export interface SocketLike {
  onopen: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  readonly readyState: number;
  send(data: string): void;
  close(): void;
}

export interface TransportLike {
  start(): Promise<void>;
  close(): void;
}

export interface WatchDeps {
  socket(url: string): SocketLike;
  transport(params: WatchParams): TransportLike;
}

export interface WatchView {
  status(status: WatchStatus): void;
  sharer(name: string): void;
  stream(stream: MediaStream): void;
}

interface Watching { share: string; sharer: string; preset: string; ice_servers: IceServer[] }

type Pending = { resolve(ok: unknown): void; reject(code: string): void };

const OPEN = 1;

export class WatchSession {
  private socket: SocketLike | null = null;
  private transport: TransportLike | null = null;
  private ping: ReturnType<typeof setInterval> | null = null;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private current: WatchStatus = "connecting";
  private share: string | null = null;

  constructor(private readonly view: WatchView, private readonly deps: WatchDeps) {}

  get status(): WatchStatus { return this.current; }

  start(endpoint: string | null, token: string | null): void {
    if (!token || !endpoint) return this.finish("invalid");
    this.set("connecting");
    let socket: SocketLike;
    try { socket = this.deps.socket(endpoint); } catch { return this.finish("lost"); }
    this.socket = socket;
    socket.onopen = () => {
      this.request("guest.watch", { token }).then(ok => this.watch(ok), code => this.refused(code));
    };
    socket.onmessage = ev => this.message(ev.data);
    socket.onclose = () => this.finish("lost");
    socket.onerror = () => {};
  }

  /** Ends the session on this side, e.g. when the page is left. */
  close(): void {
    this.finish(FINAL.has(this.current) ? this.current : "lost", false);
  }

  private set(status: WatchStatus): void {
    if (FINAL.has(this.current) || this.current === status) return;
    this.current = status;
    this.view.status(status);
  }

  private finish(status: WatchStatus, report = true): void {
    if (report) this.set(status);
    else if (!FINAL.has(this.current)) this.current = status;
    if (this.ping) { clearInterval(this.ping); this.ping = null; }
    for (const p of this.pending.values()) p.reject("closed");
    this.pending.clear();
    const transport = this.transport, socket = this.socket;
    this.transport = null;
    this.socket = null;
    transport?.close();
    if (socket) {
      socket.onopen = socket.onmessage = socket.onclose = socket.onerror = null;
      socket.close();
    }
  }

  private request(op: string, d: object): Promise<unknown> {
    const socket = this.socket;
    if (!socket || socket.readyState !== OPEN) return Promise.reject("closed");
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      socket.send(JSON.stringify({ id, op, d }));
    });
  }

  private message(data: unknown): void {
    if (typeof data !== "string") return;
    let frame: unknown;
    try { frame = JSON.parse(data); } catch { return; }
    if (!isRecord(frame)) return;
    if (typeof frame.re === "number") {
      const p = this.pending.get(frame.re);
      if (!p) return;
      this.pending.delete(frame.re);
      if ("ok" in frame) p.resolve(frame.ok);
      else p.reject(isRecord(frame.err) && typeof frame.err.code === "string" ? frame.err.code : "internal");
      return;
    }
    if (typeof frame.ev !== "string" || !isRecord(frame.d) || frame.d.share !== this.share) return;
    switch (frame.ev) {
      case "screen.stopped": return this.finish(frame.d.reason === "codec" ? "codec" : "ended");
      case "screen.link": if (frame.d.active === false) this.finish("off"); return;
      // From the server's budget (PR 3b): a paused guest keeps its page and waits.
      case "screen.paused": return this.set(frame.d.reason === "budget" ? "paused" : "slow");
      case "screen.resumed": if (this.current === "slow" || this.current === "paused") this.set("watching"); return;
    }
  }

  private refused(code: string): void {
    if (code === "closed") return;
    this.finish(code === "not_found" ? "invalid" : code === "busy" ? "busy" : "lost");
  }

  private watch(ok: unknown): void {
    if (!isWatching(ok)) return this.finish("lost");
    this.share = ok.share;
    this.view.sharer(ok.sharer);
    // The server takes pings only once the guest is admitted.
    this.ping = setInterval(() => { this.request("ping", {}).catch(() => {}); }, PING_INTERVAL_MS);
    if (typeof RTCPeerConnection === "undefined") return this.finish("unsupported");
    const transport = this.deps.transport({
      run: 0,
      slots: 1,
      iceServers: ok.ice_servers,
      offer: async sdp => {
        const answer = await this.request("guest.offer", { sdp, browser: true });
        if (!isRecord(answer) || typeof answer.sdp !== "string") throw new Error("no answer");
        return answer.sdp;
      },
      state: (state: TransportState) => {
        if (state === "failed") this.finish("lost");
        else if (state === "connected" && this.current === "connecting") this.set("watching");
      },
      track: (slot, stream) => { if (slot === 0) this.view.stream(stream); },
    });
    this.transport = transport;
    transport.start().catch(() => { if (this.transport === transport) this.finish("lost"); });
  }
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function isWatching(v: unknown): v is Watching {
  return isRecord(v) && typeof v.share === "string" && typeof v.sharer === "string" && v.sharer.length <= 256
    && typeof v.preset === "string" && Array.isArray(v.ice_servers)
    && v.ice_servers.every(s => isRecord(s) && Array.isArray(s.urls) && s.urls.every(u => typeof u === "string"));
}
