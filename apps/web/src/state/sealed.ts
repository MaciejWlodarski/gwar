import { tNow } from "../i18n";
import type { MlsRuntime } from "../mls/runtime";
import type { Link } from "../net/connection";
import { ConnectionLostError, RequestError } from "../net/connection";
import type { Identity } from "../net/identity";
import { encodeBase64Url } from "../net/base64url";
import { DELETION_TEXT, MessageVerifier, fullyVerified, notificationAllowed, sameMessageIdentity, signMessage } from "../net/message-signature";
import { withChannelLifecycle } from "../net/channel-lifecycle";
import { sameAttachments } from "../net/sealed-file";
import type { Attachment } from "../proto/Attachment";
import type { MessageSignature } from "../proto/MessageSignature";
import type { SealedMessage } from "../proto/SealedMessage";
import type { ChatMessage } from "../proto/ChatMessage";
import { conversationThread, sealedThread, threadConversation } from "../lib/chat";
import { latestMessageId, storedKey, type SealedThreadKey } from "./reducer";
import { useSession } from "./stores";
import type { ChatItem } from "./types";

export type SealedRuntime = Pick<MlsRuntime, "status" | "subscribe" | "openConversation" | "sync" | "sealMessage" | "openMessage">;
/** A sealed thread: a Gwar direct message, encrypted channel or server chat. */
export type SealedKey = SealedThreadKey;
type PublicKey = Exclude<SealedKey, `dm:${string}`>;
/** The server caps unread counts at this; mention scans stop there too. */
export const UNREAD_CAP = 100;
/** Public conversations scanned for mentions at the same time. */
const SCANS = 4;
/** Rows a mention scan reads at most: the server keeps this many per conversation. */
const SCAN_ROWS = 1000;

/** One session's sealed chat work, for direct messages and public encrypted chat.
 * Every asynchronous boundary checks its session fence. */
export class SealedConversations {
  private queue = Promise.resolve();
  /** Live rows still to notify once opened (by item key), whichever path opens them. */
  private pending = new Set<string>();
  private notified = new Set<string>();
  private loading = new Set<string>();
  /** Rows of fetched pages and queued or running opens, per `conversation:id`. */
  private queued = new Map<string, number>();
  /** Fetches and opens running per conversation; while any runs, its removals
   * are remembered (tombstones), so a response already on its way cannot restore a row. */
  private busy = new Map<string, number>();
  private removed = new Map<string, Set<number>>();
  private opening = new Set<string>();
  /** Running unread recounts per thread, with the ids removed since each started. */
  private reading = new Map<string, Set<number>>();
  /** Per public thread and signed message identity (`author_uid:message_id`),
   * the newest accepted revision's numeric id and whether it mentions me
   * (docs/e2e-build.md, "Mention accounting"). Pruned at the read mark. */
  private contributions = new Map<string, Map<string, { id: number; mention: boolean }>>();
  /** Per thread, the newest message that was on screen: read here before the server's mark catches up. */
  private seen = new Map<string, number>();
  /** Running scans per `epoch:thread`, and threads waiting for one of the SCANS slots. */
  private scanning = new Set<string>();
  private scans: PublicKey[] = [];
  private scanners = 0;
  /** Threads whose scan stopped at UNREAD_CAP mentions. */
  private capped = new Set<string>();
  private unsubscribe: () => void;
  private stopped = false;
  private verifier = new MessageVerifier();
  constructor(private runtime: SealedRuntime, private link: Link, private identity: Identity,
    private uid: string, private current: () => boolean, private notify: (msg: ChatMessage) => void,
    private error: (error: unknown) => void) {
    const runtimeChanges = runtime.subscribe(() => this.refresh());
    const viewChanges = useSession.subscribe((s, prev) => {
      if (s.activeThread === prev.activeThread && s.viewChannel === prev.viewChannel && s.focused === prev.focused) return;
      const key = storedKey(s, s.activeThread);
      if (key === "server" || key?.startsWith("ch:")) this.recount(key);
    });
    this.unsubscribe = () => { runtimeChanges(); viewChanges(); };
  }
  remove(conversation: string, ids: number[]): void {
    if (this.busy.has(conversation)) {
      const removed = this.removed.get(conversation) ?? new Set<number>();
      for (const id of ids) removed.add(id);
      this.removed.set(conversation, removed);
    }
    const thread = conversationThread(conversation, this.uid);
    const contributions = thread && this.contributions.get(thread);
    if (!thread || !contributions) return;
    for (const [identity, c] of contributions) if (ids.includes(c.id)) contributions.delete(identity);
    this.recount(thread);
  }
  close(): void { this.stopped = true; this.unsubscribe(); this.pending.clear(); }
  /** A Welcome rebuilt the threads: mention accounting starts over from its read marks. */
  reset(): void { this.contributions.clear(); this.capped.clear(); this.scans = []; }
  private check(): void { if (this.stopped || !this.current()) throw new ConnectionLostError(); }
  conversation(thread: SealedKey): string { return threadConversation(thread, this.uid); }
  refresh(): void {
    if (this.stopped || !this.current()) return;
    const state = useSession.getState();
    for (const key of Object.keys(state.threads)) {
      if (!sealedThread(key, state.kind, state.channels, state.server?.server_chat)) continue;
      state.dispatch({ type: "sealedStatus", thread: key as SealedKey, securing: this.runtime.status(this.conversation(key as SealedKey)) !== "ready" });
    }
    void this.retryMissing();
  }
  private retrying = false;
  private retryAgain = false;
  /** Opens rows that were waiting for a key, one at a time: however many wait,
   * each pass reaches every one without filling the verification queue. */
  private async retryMissing(): Promise<void> {
    if (this.retrying) { this.retryAgain = true; return; }
    this.retrying = true;
    try {
      do {
        this.retryAgain = false;
        const state = useSession.getState(), epoch = state.epoch;
        const rows = Object.entries(state.threads).filter(([key]) => sealedThread(key, state.kind, state.channels, state.server?.server_chat))
          .flatMap(([key, thread]) => thread.items.flatMap(i => i.kind === "msg" && i.sealed && i.cannotOpen === "no_key" ? [[key, i.key, i.sealed.revision] as const] : []));
        for (const [thread, key, revision] of rows) {
          if (this.stopped || !this.current()) return;
          // A Welcome rebuilt the threads: start a fresh pass over them.
          if (useSession.getState().epoch !== epoch) { this.retryAgain = true; break; }
          // Only a row still there and still waiting: it may have been removed or opened meanwhile.
          const item = useSession.getState().threads[thread]?.items.find(i => i.key === key);
          if (item?.kind !== "msg" || !item.sealed || item.cannotOpen !== "no_key" || item.sealed.revision !== revision) continue;
          await this.receive(item.sealed, false, epoch).catch(this.error);
        }
      } while (this.retryAgain);
    } finally { this.retrying = false; }
  }
  async open(thread: SealedKey, before?: number): Promise<void> {
    const epoch = useSession.getState().epoch;
    const check = () => { this.check(); if (epoch !== useSession.getState().epoch) throw new ConnectionLostError(); };
    const token = `${epoch}:${thread}:${before ?? "latest"}`;
    if (this.loading.has(token)) return;
    this.loading.add(token);
    const conversation = this.conversation(thread);
    this.enter(conversation);
    try {
      check();
      // A direct message's group is created on first use; the runtime keeps public groups itself.
      if (thread.startsWith("dm:")) await this.runtime.openConversation(conversation);
      check(); this.refresh();
      const page = await this.link.request("chat.sealed.history", { conversation, before: before ?? null, limit: 50 });
      check();
      await this.page(conversation, page.sealed, before ?? null, epoch);
      check();
      // Byte-limited pages may be short; only an empty page proves the end.
      useSession.getState().dispatch({ type: "sealedHistory", thread, hasMore: page.sealed.length > 0 });
    } finally { this.loading.delete(token); this.leave(conversation); }
  }
  /** Opens one fetched page in order. Its rows are held until processed: a
   * removal arriving meanwhile must not be forgotten. */
  private async page(conversation: string, rows: SealedMessage[], before: number | null, epoch: number): Promise<void> {
    const held = rows.map(row => `${row.conversation}:${row.id}`);
    for (const k of held) this.hold(k);
    try {
      for (const row of rows) {
        this.check(); if (epoch !== useSession.getState().epoch) throw new ConnectionLostError();
        if (row.conversation !== conversation || (before !== null && row.id >= before)) throw new Error("Invalid conversation history cursor");
        await this.receive(row, false, epoch);
      }
    } finally { for (const k of held) this.release(k); this.prune(); }
  }
  /** A read event can advance through rows this device has never loaded. Count
   * ciphertext metadata in bounded pages, without opening or notifying. */
  async refreshUnread(conversation: string, removed: number[] = []): Promise<void> {
    const thread = conversationThread(conversation, this.uid);
    if (!thread) return;
    // A running pass may already have counted rows removed since: it drops them before counting.
    const epoch = useSession.getState().epoch, pass = `${epoch}:${thread}`;
    const running = this.reading.get(pass);
    if (running) { for (const id of removed) running.add(id); return; }
    const gone = new Set(removed);
    this.reading.set(pass, gone);
    try {
      let before: number | null = null, through = 0;
      const ids = new Set<number>();
      for (;;) {
        this.check(); if (epoch !== useSession.getState().epoch) return;
        const page: { sealed: SealedMessage[] } = await this.link.request("chat.sealed.history", { conversation, before, limit: 100 });
        this.check(); if (epoch !== useSession.getState().epoch) return;
        const mark = useSession.getState().threads[thread]?.lastRead ?? 0;
        for (const row of page.sealed) {
          if (row.conversation !== conversation || (before !== null && row.id >= before)) throw new Error("Invalid conversation history cursor");
          ids.add(row.id); through = Math.max(through, row.id);
        }
        const unread = [...ids].filter(id => id > mark && !gone.has(id)).length;
        const oldest = page.sealed.length ? Math.min(...page.sealed.map(row => row.id)) : 0;
        if (unread >= UNREAD_CAP || !oldest || oldest <= mark) {
          useSession.getState().dispatch({ type: "sealedUnread", thread, count: Math.min(UNREAD_CAP, unread), through });
          this.recount(thread);
          return;
        }
        before = oldest;
        await new Promise(resolve => setTimeout(resolve, 100));
      }
    } finally { this.reading.delete(pass); }
  }
  /** After connecting: opens the unread messages of every public encrypted conversation to
   * count those mentioning me, at most SCANS conversations at a time, without notifying. */
  scanUnread(): Promise<void> {
    const state = useSession.getState();
    return this.queueScans(Object.entries(state.threads)
      .filter(([key, t]) => (key === "server" || key.startsWith("ch:")) && t.unread > 0 && sealedThread(key, state.kind, state.channels, state.server?.server_chat))
      .map(([key]) => key as PublicKey));
  }
  /** Every scan, also a rescan after a cap, waits for one of SCANS slots. */
  private async queueScans(threads: PublicKey[]): Promise<void> {
    for (const thread of threads) if (!this.scans.includes(thread)) this.scans.push(thread);
    const workers: Promise<void>[] = [];
    for (; this.scanners < SCANS && this.scans.length; this.scanners++) workers.push((async () => {
      try { for (let thread = this.scans.shift(); thread && !this.stopped; thread = this.scans.shift()) await this.scan(thread).catch(this.error); }
      finally { this.scanners--; }
    })());
    await Promise.all(workers);
  }
  /** Pages backwards through a public conversation's unread rows until its read mark, the
   * oldest retained row or UNREAD_CAP mentions, opening them as history. */
  private async scan(thread: PublicKey): Promise<void> {
    const epoch = useSession.getState().epoch, pass = `${epoch}:${thread}`;
    if (this.scanning.has(pass)) return;
    this.scanning.add(pass);
    const conversation = this.conversation(thread);
    this.enter(conversation);
    try {
      let before: number | null = null;
      for (let rows = 0; rows < SCAN_ROWS;) {
        this.check(); if (epoch !== useSession.getState().epoch) return;
        const page: { sealed: SealedMessage[] } = await this.link.request("chat.sealed.history", { conversation, before, limit: 100 });
        if (!page.sealed.length) return;
        await this.page(conversation, page.sealed, before, epoch);
        rows += page.sealed.length;
        const oldest = Math.min(...page.sealed.map(row => row.id));
        if (oldest <= this.mark(thread)) return;
        if (this.mentions(thread) >= UNREAD_CAP) { this.capped.add(thread); return; }
        before = oldest;
      }
    } finally { this.scanning.delete(pass); this.leave(conversation); }
  }
  /** What I have read in a thread: the server's mark, or what has been on screen since. */
  private mark(thread: string): number {
    const s = useSession.getState(), t = s.threads[thread];
    if (!t) return 0;
    if (s.focused && storedKey(s, s.activeThread) === thread) this.seen.set(thread, Math.max(this.seen.get(thread) ?? 0, latestMessageId(t)));
    return Math.max(t.lastRead, this.seen.get(thread) ?? 0);
  }
  /** Unread messages mentioning me; contributions at or below the read mark are dropped for good. */
  private mentions(thread: string): number {
    const mark = this.mark(thread), contributions = this.contributions.get(thread);
    let count = 0;
    for (const [identity, c] of contributions ?? []) {
      if (c.id <= mark) contributions!.delete(identity);
      else if (c.mention) count++;
    }
    return count;
  }
  private recount(thread: string): void {
    const state = useSession.getState();
    if (this.stopped || (thread !== "server" && !thread.startsWith("ch:")) || !state.threads[thread]
      || !sealedThread(thread, state.kind, state.channels, state.server?.server_chat)) return;
    const count = this.mentions(thread);
    useSession.getState().dispatch({ type: "sealedMentions", thread: thread as PublicKey, count: Math.min(UNREAD_CAP, count) });
    // An edit or removal took a capped scan's count below the cap: look further back.
    if (count < UNREAD_CAP && this.capped.delete(thread)) void this.queueScans([thread as PublicKey]);
  }
  /** Sets a public message's mention contribution from its newest accepted
   * revision; an unopened or rejected revision leaves the previous one. */
  private account(thread: SealedKey, row: SealedMessage, item: Extract<ChatItem, { kind: "msg" }>): void {
    if (thread.startsWith("dm:")) return;
    const contributions = this.contributions.get(thread) ?? new Map<string, { id: number; mention: boolean }>();
    this.contributions.set(thread, contributions);
    const identity = `${row.author_uid}:${row.message_id}`;
    if (row.deleted) contributions.delete(identity);
    else if (item.authentic) contributions.set(identity, { id: row.id,
      mention: !!item.verified && row.author_uid !== this.uid && (item.msg.mentions ?? []).includes(this.uid) });
    else return;
    this.recount(thread);
  }
  private hold(rowKey: string): void { this.queued.set(rowKey, (this.queued.get(rowKey) ?? 0) + 1); }
  private release(rowKey: string): void {
    const left = this.queued.get(rowKey)! - 1;
    if (left) this.queued.set(rowKey, left); else this.queued.delete(rowKey);
  }
  private enter(conversation: string): void { this.busy.set(conversation, (this.busy.get(conversation) ?? 0) + 1); }
  private leave(conversation: string): void {
    const left = this.busy.get(conversation)! - 1;
    if (left) this.busy.set(conversation, left);
    else { this.busy.delete(conversation); this.removed.delete(conversation); }
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
    this.hold(rowKey); this.enter(row.conversation);
    const wasRemoved = () => !!this.removed.get(row.conversation)?.has(row.id);
    const thread = conversationThread(row.conversation, this.uid);
    // A channel deleted meanwhile keeps no thread.
    const gone = () => { const s = useSession.getState(); return !thread || !sealedThread(thread, s.kind, s.channels, s.server?.server_chat); };
    const run = this.queue.then(async () => {
      this.check(); if (epoch !== useSession.getState().epoch || wasRemoved()) return;
      const state = useSession.getState();
      if (!thread || gone()) return;
      const channel = thread.startsWith("ch:") ? Number(thread.slice(3)) : null;
      const peer = thread.startsWith("dm:") ? thread.slice(3) : null;
      if (peer && ![peer, this.uid].includes(row.author_uid)) return;
      const old = state.threads[thread]?.items.find(i => i.key === key);
      if (old?.kind === "msg" && (old.sealed?.revision ?? -1) > row.revision) return;
      const result = await this.runtime.openMessage(row);
      this.check(); if (epoch !== useSession.getState().epoch || wasRemoved() || gone()) return;
      const msg: ChatMessage = { id: row.id, target: thread === "server" ? "server" : channel === null ? { client: 0 } : { channel }, author: 0, author_uid: row.author_uid,
        author_name: row.author_name, sent_at: row.sent_at, edited_at: row.edited_at, text: "" };
      let item: Extract<ChatItem, { kind: "msg" }> = { kind: "msg", key, at: row.sent_at, msg, sealed: row };
      if ("ok" in result) {
        Object.assign(msg, result.ok);
        const serverKey = this.link.serverKey!;
        const verification = withChannelLifecycle(await this.verifier.verify(msg, { serverKey, target: row.conversation,
          participants: peer ? [this.uid, peer] : undefined, sealed: { group: row.group, epoch: row.epoch } }), serverKey, row.conversation);
        this.check(); if (epoch !== useSession.getState().epoch || wasRemoved() || gone()) return;
        if (verification.continuity === "fork" || verification.continuity === "rollback") return;
        if (verification.status !== "verified" || row.deleted !== (msg.text === DELETION_TEXT)) {
          item = { ...item, msg: { ...msg, text: "", signature: null, mentions: [], attachments: [] }, cannotOpen: "cannot_open", removed: row.deleted };
        } else {
          if (old?.kind === "msg" && old.authentic && !sameMessageIdentity(old.msg, msg)) return;
          // An edit cannot swap the files its message was sent with: such a revision is ignored, like a fork.
          if (old?.kind === "msg" && old.authentic && msg.text !== DELETION_TEXT && !sameAttachments(old.msg.attachments ?? [], msg.attachments ?? [])) return;
          item = { ...item, authentic: true, verified: fullyVerified(verification) };
          if (this.pending.has(key) && !this.notified.has(key) && notificationAllowed(verification)) this.notify(msg);
          this.notified.add(key); this.pending.delete(key);
        }
      } else {
        item = { ...item, cannotOpen: result.reason, removed: row.deleted };
      }
      useSession.getState().dispatch({ type: "sealed", thread, item, live });
      this.account(thread, row, item);
    });
    const done = run.finally(() => { this.opening.delete(token); this.release(rowKey); this.leave(row.conversation); this.prune(); });
    this.queue = done.catch(() => {});
    return done;
  }
  /** Sends a message, or a revision of `original`. Its encrypted attachments are
   * tied to it when it is sent and stay its own through every edit; a deletion lists none. */
  async send(thread: SealedKey, text: string, mentions: string[], original?: MessageSignature, attachments: Attachment[] = []): Promise<void> {
    this.check();
    const epoch = useSession.getState().epoch;
    const checkSession = () => { this.check(); if (epoch !== useSession.getState().epoch) throw new ConnectionLostError(); };
    const check = () => {
      checkSession();
      if (original) {
        const item = useSession.getState().threads[thread]?.items.find(i => i.kind === "msg" && i.msg.signature?.message_id === original.message_id);
        if (item?.kind === "msg" && (item.cannotOpen || item.msg.signature?.revision !== original.revision)) throw new Error(tNow("chat.editChanged"));
      }
    };
    const conversation = this.conversation(thread);
    const retry = { message_id: original?.message_id ?? encodeBase64Url(crypto.getRandomValues(new Uint8Array(16))), created_at: original?.created_at ?? Date.now() };
    if (text === DELETION_TEXT) attachments = [];
    if (attachments.some(a => !a.sealed)) throw new Error("Unencrypted attachment in an encrypted conversation");
    if (this.runtime.status(conversation) !== "ready") throw new Error(tNow("chat.securing"));
    for (let attempt = 0; attempt < 3; attempt++) {
      let signature!: MessageSignature;
      const sealed = await this.runtime.sealMessage(conversation, async ctx => {
        check();
        signature = await signMessage(this.identity, { serverKey: ctx.server_key, target: conversation,
          sealed: { group: ctx.group_id, epoch: Number(ctx.epoch) } }, this.uid, text, mentions, attachments, original, retry);
        check(); return { text, mentions, attachments, signature };
      });
      check();
      try {
        const row = original
          ? await this.link.request(text === DELETION_TEXT ? "chat.delete_own" : "chat.sealed.edit", { conversation, ...sealed, message_id: retry.message_id, revision: signature.revision })
          : await this.link.request("chat.sealed.send", { conversation, ...sealed, message_id: retry.message_id, files: attachments.map(a => a.id) });
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
