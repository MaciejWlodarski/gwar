import { tNow } from "../i18n";
import type { MlsRuntime } from "../mls/runtime";
import type { Link } from "../net/connection";
import { ConnectionLostError, RequestError } from "../net/connection";
import type { Identity } from "../net/identity";
import { encodeBase64Url } from "../net/base64url";
import { DELETION_TEXT, MessageVerifier, fullyVerified, notificationAllowed, sameMessageIdentity, signMessage } from "../net/message-signature";
import type { MessageSignature } from "../proto/MessageSignature";
import type { SealedMessage } from "../proto/SealedMessage";
import type { ChatMessage } from "../proto/ChatMessage";
import { dmPeer, messageTarget } from "../lib/chat";
import { useSession } from "./stores";
import type { ChatItem } from "./types";

export type DmRuntime = Pick<MlsRuntime, "status" | "subscribe" | "openConversation" | "sync" | "sealMessage" | "openMessage">;
/** One session's sealed chat work. Every asynchronous boundary checks its session fence. */
export class DirectMessages {
  private queue = Promise.resolve();
  /** Live rows still to notify once opened (by item key), whichever path opens them. */
  private pending = new Set<string>();
  private notified = new Set<string>();
  private loading = new Set<string>();
  /** Rows of fetched pages and queued or running opens, per `conversation:id`;
   * removals are only remembered for these. */
  private queued = new Map<string, number>();
  private removed = new Set<string>();
  private opening = new Set<string>();
  /** Running unread recounts per peer, with the ids removed since each started. */
  private reading = new Map<string, Set<number>>();
  private unsubscribe: () => void;
  private stopped = false;
  private verifier = new MessageVerifier();
  constructor(private runtime: DmRuntime, private link: Link, private identity: Identity,
    private uid: string, private current: () => boolean, private notify: (msg: ChatMessage) => void,
    private error: (error: unknown) => void) {
    this.unsubscribe = runtime.subscribe(() => this.refresh());
  }
  remove(conversation: string, ids: number[]): void {
    for (const id of ids) if (this.queued.has(`${conversation}:${id}`)) this.removed.add(`${conversation}:${id}`);
  }
  close(): void { this.stopped = true; this.unsubscribe(); this.pending.clear(); }
  private check(): void { if (this.stopped || !this.current()) throw new ConnectionLostError(); }
  conversation(peer: string): string { return messageTarget(`dm:${peer}`, this.uid, this.uid); }
  refresh(): void {
    if (this.stopped || !this.current()) return;
    const state = useSession.getState();
    for (const [key, thread] of Object.entries(state.threads)) {
      if (!key.startsWith("dm:") || key.startsWith("dm:ts:")) continue;
      const peer = key.slice(3);
      state.dispatch({ type: "dmStatus", peer, securing: this.runtime.status(this.conversation(peer)) !== "ready" });
      for (const item of thread.items) if (item.kind === "msg" && item.sealed && item.cannotOpen === "no_key") {
        void this.receive(item.sealed, false).catch(this.error);
      }
    }
  }
  async open(peer: string, before?: number): Promise<void> {
    const epoch = useSession.getState().epoch;
    const check = () => { this.check(); if (epoch !== useSession.getState().epoch) throw new ConnectionLostError(); };
    const token = `${epoch}:${peer}:${before ?? "latest"}`;
    if (this.loading.has(token)) return;
    this.loading.add(token);
    try {
      check();
      const conversation = this.conversation(peer);
      await this.runtime.openConversation(conversation);
      check(); this.refresh();
      const page = await this.link.request("chat.sealed.history", { conversation, before: before ?? null, limit: 50 });
      check();
      // Held until processed: a removal arriving meanwhile must not be forgotten.
      const held = page.sealed.map(row => `${row.conversation}:${row.id}`);
      for (const k of held) this.hold(k);
      try {
        for (const row of page.sealed) {
          check();
          if (row.conversation !== conversation || (before !== undefined && row.id >= before)) throw new Error("Invalid conversation history cursor");
          await this.receive(row, false, epoch);
        }
      } finally { for (const k of held) this.release(k); this.prune(); }
      check();
      // Byte-limited pages may be short; only an empty page proves the end.
      useSession.getState().dispatch({ type: "dmHistory", peer, hasMore: page.sealed.length > 0 });
    } finally { this.loading.delete(token); }
  }
  /** A read event can advance through rows this device has never loaded. Count
   * ciphertext metadata in bounded pages, without opening or notifying. */
  async refreshUnread(conversation: string, removed: number[] = []): Promise<void> {
    const peer = dmPeer(conversation, this.uid);
    if (!peer) return;
    // A running pass may already have counted rows removed since: it drops them before counting.
    const running = this.reading.get(peer);
    if (running) { for (const id of removed) running.add(id); return; }
    const gone = new Set(removed);
    this.reading.set(peer, gone);
    const epoch = useSession.getState().epoch;
    try {
      let before: number | null = null, through = 0;
      const ids = new Set<number>();
      for (;;) {
        this.check(); if (epoch !== useSession.getState().epoch) return;
        const page: { sealed: SealedMessage[] } = await this.link.request("chat.sealed.history", { conversation, before, limit: 100 });
        this.check(); if (epoch !== useSession.getState().epoch) return;
        const mark = useSession.getState().threads[`dm:${peer}`]?.lastRead ?? 0;
        for (const row of page.sealed) {
          if (row.conversation !== conversation || (before !== null && row.id >= before)) throw new Error("Invalid conversation history cursor");
          ids.add(row.id); through = Math.max(through, row.id);
        }
        const unread = [...ids].filter(id => id > mark && !gone.has(id)).length;
        const oldest = page.sealed.length ? Math.min(...page.sealed.map(row => row.id)) : 0;
        if (unread >= 100 || !oldest || oldest <= mark) {
          useSession.getState().dispatch({ type: "dmUnread", peer, count: Math.min(100, unread), through });
          return;
        }
        before = oldest;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
    } finally { this.reading.delete(peer); }
  }
  private hold(rowKey: string): void { this.queued.set(rowKey, (this.queued.get(rowKey) ?? 0) + 1); }
  private release(rowKey: string): void {
    const left = this.queued.get(rowKey)! - 1;
    if (left) this.queued.set(rowKey, left);
    else { this.queued.delete(rowKey); this.removed.delete(rowKey); }
  }
  /** Bound session metadata to retained rows and rows still being processed. */
  private prune(): void {
    const keys = new Set(Object.values(useSession.getState().threads).flatMap(t => t.items.map(i => i.key)));
    for (const k of this.queued.keys()) keys.add(`e${k.slice(k.lastIndexOf(":") + 1)}`);
    for (const k of this.pending) if (!keys.has(k)) this.pending.delete(k);
    for (const k of this.notified) if (!keys.has(k)) this.notified.delete(k);
  }
  receive(row: SealedMessage, live: boolean, epoch = useSession.getState().epoch): Promise<void> {
    const key = `e${row.id}`;
    const token = `${epoch}:${JSON.stringify(row)}`;
    const duplicate = this.opening.has(token);
    if (!duplicate && this.opening.size >= 256) return Promise.reject(new Error("Conversation verification queue full"));
    // Recorded before deduplication: a history open of the same row must still notify.
    if (live && row.revision === 0) this.pending.add(key);
    if (duplicate) return this.queue;
    this.opening.add(token);
    const rowKey = `${row.conversation}:${row.id}`;
    this.hold(rowKey);
    const wasRemoved = () => this.removed.has(rowKey);
    const run = this.queue.then(async () => {
      this.check(); if (epoch !== useSession.getState().epoch || wasRemoved()) return;
      const peer = dmPeer(row.conversation, this.uid);
      if (!peer || ![peer, this.uid].includes(row.author_uid)) return;
      const old = useSession.getState().threads[`dm:${peer}`]?.items.find(i => i.key === key);
      if (old?.kind === "msg" && (old.sealed?.revision ?? -1) > row.revision) return;
      const result = await this.runtime.openMessage(row);
      this.check(); if (epoch !== useSession.getState().epoch || wasRemoved()) return;
      const msg: ChatMessage = { id: row.id, target: { client: 0 }, author: 0, author_uid: row.author_uid,
        author_name: row.author_name, sent_at: row.sent_at, edited_at: row.edited_at, text: "" };
      let item: Extract<ChatItem, { kind: "msg" }> = { kind: "msg", key, at: row.sent_at, msg, sealed: row };
      if ("ok" in result) {
        Object.assign(msg, result.ok);
        const verification = await this.verifier.verify(msg, { serverKey: this.link.serverKey!, target: row.conversation,
          participants: [this.uid, peer], sealed: { group: row.group, epoch: row.epoch } });
        this.check(); if (epoch !== useSession.getState().epoch || wasRemoved()) return;
        if (verification.continuity === "fork" || verification.continuity === "rollback") return;
        if (verification.status !== "verified" || row.deleted !== (msg.text === DELETION_TEXT)) {
          item = { ...item, msg: { ...msg, text: "", signature: null, mentions: [], attachments: [] }, cannotOpen: "cannot_open", removed: row.deleted };
        } else {
          if (old?.kind === "msg" && old.authentic && !sameMessageIdentity(old.msg, msg)) return;
          item = { ...item, authentic: true, verified: fullyVerified(verification) };
          if (this.pending.has(key) && !this.notified.has(key) && notificationAllowed(verification)) this.notify(msg);
          this.notified.add(key); this.pending.delete(key);
        }
      } else {
        item = { ...item, cannotOpen: result.reason, removed: row.deleted };
      }
      useSession.getState().dispatch({ type: "sealed", peer, item, live });
    });
    const done = run.finally(() => { this.opening.delete(token); this.release(rowKey); this.prune(); });
    this.queue = done.catch(() => {});
    return done;
  }
  async send(peer: string, text: string, mentions: string[], original?: MessageSignature): Promise<void> {
    this.check();
    const epoch = useSession.getState().epoch;
    const checkSession = () => { this.check(); if (epoch !== useSession.getState().epoch) throw new ConnectionLostError(); };
    const check = () => {
      checkSession();
      if (original) {
        const item = useSession.getState().threads[`dm:${peer}`]?.items.find(i => i.kind === "msg" && i.msg.signature?.message_id === original.message_id);
        if (item?.kind === "msg" && (item.cannotOpen || item.msg.signature?.revision !== original.revision)) throw new Error(tNow("chat.editChanged"));
      }
    };
    const conversation = this.conversation(peer);
    const retry = { message_id: original?.message_id ?? encodeBase64Url(crypto.getRandomValues(new Uint8Array(16))), created_at: original?.created_at ?? Date.now() };
    if (this.runtime.status(conversation) !== "ready") throw new Error(tNow("chat.securing"));
    for (let attempt = 0; attempt < 3; attempt++) {
      let signature!: MessageSignature;
      const sealed = await this.runtime.sealMessage(conversation, async ctx => {
        check();
        signature = await signMessage(this.identity, { serverKey: ctx.server_key, target: conversation,
          sealed: { group: ctx.group_id, epoch: Number(ctx.epoch) } }, this.uid, text, mentions, [], original, retry);
        check(); return { text, mentions, attachments: [], signature };
      });
      check();
      try {
        const row = original
          ? await this.link.request(text === DELETION_TEXT ? "chat.delete_own" : "chat.sealed.edit", { conversation, ...sealed, message_id: retry.message_id, revision: signature.revision })
          : await this.link.request("chat.sealed.send", { conversation, ...sealed, message_id: retry.message_id });
        checkSession(); await this.receive(row, false); return;
      } catch (error) {
        checkSession();
        if (!(error instanceof RequestError) || !["stale", "pending"].includes(error.code) || attempt === 2) throw error;
        await this.runtime.sync(); check();
        if (this.runtime.status(conversation) !== "ready") throw new Error(tNow("chat.securing"), { cause: error });
      }
    }
  }
}
