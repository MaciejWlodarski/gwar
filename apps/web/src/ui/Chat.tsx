import { AlertCircle, ArrowDown, Check, Hash, Loader2, Lock, Menu, Megaphone, MessageSquare, Paperclip, Pencil, RotateCw, Send, Trash2, Users, X } from "lucide-react";
import { Fragment, memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ClipboardEvent, type DragEvent, type KeyboardEvent, type MouseEvent } from "react";
import { countKey, useLanguage, useT, type TFn } from "../i18n";
import { cn } from "../lib/cn";
import { memberName } from "../lib/member-name";
import { buildRows, type Row } from "../lib/chat";
import { useIsMobile } from "../lib/media";
import { checkUpload, formatBytes } from "../lib/files";
import { splitMentions } from "../lib/mentions";
import { tokenizeText } from "../lib/text";
import { MAX_MESSAGE_LENGTH } from "../net/protocol";
import type { ChatMessage } from "../proto/ChatMessage";
import { controller, UploadError } from "../state/controller";
import { myChannelId, storedKey } from "../state/reducer";
import { useOutbox, useSession, useUi } from "../state/stores";
import type { ThreadKey } from "../state/types";
import { AttachmentList } from "./Attachments";
import { ConnectBadge, MentionBadge, UnreadBadge } from "./badges";
import { usePermission, useUidColor } from "./hooks";
import { useMembersPanel } from "./members";
import { useMentionAutocomplete } from "./MentionInput";
import { DeafenButton, MicButton } from "./VoicePanel";
import { Avatar, Button, EmptyState, IconButton, Spinner, Tooltip } from "./kit";

// -------------------------------------------------------------------- text

function MessageText({ text, mentioned = [], meUid }: { text: string; mentioned?: Array<{ uid: string; nickname: string }>; meUid?: string }) {
  const tokens = useMemo(() => tokenizeText(text), [text]);
  return (
    <>
      {tokens.map((tok, i) =>
        tok.kind === "link" ? (
          <a
            key={i}
            href={tok.href}
            target="_blank"
            rel="noopener noreferrer nofollow"
            className="text-accent underline [overflow-wrap:anywhere] decoration-accent/40 underline-offset-2 hover:decoration-accent"
          >
            {tok.value}
          </a>
        ) : mentioned.length === 0 ? (
          <Fragment key={i}>{tok.value}</Fragment>
        ) : (
          <Fragment key={i}>
            {splitMentions(tok.value, mentioned).map((m, k) =>
              m.kind === "mention" ? (
                <span
                  key={k}
                  data-mention={m.uid}
                  className={cn("rounded px-0.5 font-medium", m.uid === meUid ? "bg-accent/30 text-fg" : "bg-accent-soft text-accent")}
                >
                  {m.value}
                </span>
              ) : (
                <Fragment key={k}>{m.value}</Fragment>
              ),
            )}
          </Fragment>
        ),
      )}
    </>
  );
}

/** Message text whose `@Name`s are marked; looks the people up only when the message mentions anyone. */
function RichText({ msg, meUid }: { msg: ChatMessage; meUid?: string }) {
  const members = useSession((s) => ((msg.mentions ?? []).length > 0 ? s.members : null));
  const mentioned = useMemo(
    () => (members ? (msg.mentions ?? []).flatMap((uid) => (members[uid] ? [{ uid, nickname: members[uid].nickname }] : [])) : []),
    [members, msg.mentions],
  );
  return <MessageText text={msg.text} mentioned={mentioned} meUid={meUid} />;
}

function formatTime(at: number, lang: string, short = false): string {
  return new Date(at).toLocaleTimeString(lang, { hour: short ? "numeric" : "2-digit", minute: "2-digit" });
}

function formatDay(at: number, lang: string, t: TFn): string {
  const d = new Date(at);
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  const same = (a: Date, b: Date) => a.toDateString() === b.toDateString();
  if (same(d, today)) return t("chat.today");
  if (same(d, yesterday)) return t("chat.yesterday");
  return d.toLocaleDateString(lang, {
    weekday: "long",
    day: "numeric",
    month: "long",
    ...(d.getFullYear() !== today.getFullYear() ? { year: "numeric" } : {}),
  });
}

// ----------------------------------------------------------------- messages

const MessageRow = memo(function MessageRow({
  row,
  mine,
  meUid,
  lang,
  compact,
  canEdit,
  canDelete,
  editing,
}: {
  row: Extract<Row, { type: "msg" }>;
  mine: boolean;
  meUid: string | undefined;
  lang: string;
  compact: boolean;
  canEdit: boolean;
  canDelete: boolean;
  editing: boolean;
}) {
  const t = useT();
  const { msg, at } = row.item;
  const time = formatTime(at, lang);
  const full = new Date(at).toLocaleString(lang);
  const color = useUidColor(msg.author_uid);
  const member = useSession((s) => s.members[msg.author_uid]);
  const authorName = member?.nickname ?? msg.author_name;
  const mentionsMe = !!meUid && (msg.mentions ?? []).includes(meUid);
  const attachments = msg.attachments ?? [];
  const showActions = (canEdit || canDelete) && !editing;

  const remove = (e: MouseEvent) => {
    // Shift skips the confirmation.
    if (e.shiftKey) {
      void controller.deleteMessage(msg.id);
      return;
    }
    useUi.getState().openDialog({
      kind: "confirm",
      title: t("chat.deleteTitle"),
      body: msg.text.trim() ? msg.text.slice(0, 200) : t("chat.deleteBody"),
      confirmLabel: t("common.delete"),
      danger: true,
      onConfirm: () => void controller.deleteMessage(msg.id),
    });
  };

  return (
    <div
      data-message={msg.id}
      data-mentions-me={mentionsMe || undefined}
      className={cn(
        "group relative flex gap-3 px-4 hover:bg-hover/60",
        row.first ? (compact ? "pt-2" : "pt-3") : "pt-px",
        mentionsMe && "bg-accent-soft/60 shadow-[inset_2px_0_0_var(--accent)] hover:bg-accent-soft",
        editing && "bg-hover/60",
      )}
    >
      <div className="w-9 shrink-0 pt-0.5">
        {row.first ? (
          <Avatar name={authorName} seed={msg.author_uid} size={36} />
        ) : (
          <time
            dateTime={new Date(at).toISOString()}
            title={full}
            className="hidden pt-1 text-right text-[10px] leading-5 whitespace-nowrap text-subtle group-hover:block"
          >
            {formatTime(at, lang, true)}
          </time>
        )}
      </div>
      <div className="min-w-0 flex-1 pb-px">
        {row.first && (
          <div className="flex items-baseline gap-2">
            <button className={cn("cursor-pointer text-sm font-semibold hover:underline", mine && !color && "text-accent")} style={color ? { color } : undefined}
              onClick={() => useUi.getState().openDialog({ kind: "memberProfile", uid: msg.author_uid, fallback: msg.author_name })}>
              {authorName}
            </button>
            <ConnectBadge handle={member?.connect} />
            <time dateTime={new Date(at).toISOString()} title={full} className="text-xs text-subtle">
              {time}
            </time>
          </div>
        )}
        {editing ? (
          <MessageEditor msg={msg} />
        ) : (
          <>
            {msg.text && (
              <div className="text-sm leading-[1.5] break-words whitespace-pre-wrap text-fg">
                <RichText msg={msg} meUid={meUid} />
                {msg.edited_at != null && (
                  <Tooltip label={t("chat.editedAt", { when: new Date(msg.edited_at).toLocaleString(lang) })}>
                    <span data-edited className="ml-1 cursor-default text-[11px] text-subtle">
                      {t("chat.edited")}
                    </span>
                  </Tooltip>
                )}
              </div>
            )}
            {attachments.length > 0 && <AttachmentList attachments={attachments} />}
          </>
        )}
      </div>
      {showActions && (
        <div className="absolute -top-3 right-4 flex rounded-md border border-line-strong bg-raised opacity-0 shadow-pop group-focus-within:opacity-100 group-hover:opacity-100 pointer-coarse:opacity-100">
          {canEdit && (
            <IconButton label={t("chat.edit")} size="sm" onClick={() => useUi.getState().setEditing(msg.id)}>
              <Pencil className="size-3.5" />
            </IconButton>
          )}
          {canDelete && (
            <IconButton label={t("chat.delete")} size="sm" tone="danger" onClick={remove} className="bg-transparent">
              <Trash2 className="size-3.5" />
            </IconButton>
          )}
        </div>
      )}
    </div>
  );
});

/** In-place editor for one of my messages: Enter saves, Escape cancels. */
function MessageEditor({ msg }: { msg: ChatMessage }) {
  const t = useT();
  const [text, setText] = useState(msg.text);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLTextAreaElement>(null);
  const mention = useMentionAutocomplete(text, setText, ref, msg.mentions ?? []);
  const hasFiles = (msg.attachments ?? []).length > 0;
  const over = text.length > MAX_MESSAGE_LENGTH;
  const stop = () => useUi.getState().setEditing(null);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 240)}px`;
  }, [text]);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, []);

  const save = async () => {
    const value = text.trim();
    if (busy || over || (!value && !hasFiles)) return;
    if (value === msg.text) return stop();
    setBusy(true);
    const ok = await controller.editMessage(msg.id, value, mention.mentionsFor(value));
    setBusy(false);
    if (ok) stop();
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (mention.onKeyDown(e)) return;
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      stop();
    }
  };

  return (
    <div className="relative mt-0.5">
      {mention.menu}
      <textarea
        ref={ref}
        value={text}
        rows={1}
        disabled={busy}
        aria-label={t("chat.editing")}
        onChange={(e) => {
          setText(e.target.value);
          mention.sync(e.target);
        }}
        onBeforeInput={mention.beforeInput}
        onSelect={(e) => mention.sync(e.currentTarget)}
        onKeyDown={onKeyDown}
        className={cn(
          "max-h-60 w-full resize-none rounded-lg border bg-side px-3 py-2 text-sm leading-5 text-fg outline-none focus:border-accent focus:ring-2 focus:ring-accent/25",
          over ? "border-danger" : "border-line-strong",
        )}
      />
      <div className="mt-1 flex items-center gap-2 text-xs text-subtle">
        <span>{t("chat.editHint")}</span>
        <span className="flex-1" />
        <Button size="sm" onClick={stop}>
          {t("common.cancel")}
        </Button>
        <Button size="sm" variant="primary" busy={busy} disabled={over || (!text.trim() && !hasFiles)} onClick={() => void save()}>
          <Check className="size-3.5" /> {t("common.save")}
        </Button>
      </div>
    </div>
  );
}

function SysRow({ row, t }: { row: Extract<Row, { type: "sys" }>; t: TFn }) {
  const { text } = row.item;
  const name = useSession((s) => text.uid ? memberName(s.members, text.uid, text.params?.name ?? "?") : undefined);
  if (text.key === "sys.welcome") {
    return (
      <div className="mx-4 my-3 rounded-lg border border-line bg-side px-4 py-3 text-sm">
        <div className="mb-1 flex items-center gap-2 text-xs font-medium text-muted">
          <Megaphone className="size-3.5" /> {t("chat.serverWelcome")}
        </div>
        <div className="break-words whitespace-pre-wrap text-fg">
          <MessageText text={text.params?.text ?? ""} />
        </div>
      </div>
    );
  }
  return (
    <div className="px-4 py-1 text-center text-xs text-subtle">
      {t(text.key, name === undefined ? text.params : { ...text.params, name })}
    </div>
  );
}

function MessageList({ threadKey }: { threadKey: ThreadKey }) {
  const t = useT();
  const lang = useLanguage();
  const mineUid = useSession((s) => s.me?.uid);
  const compact = false;
  const key = useSession((s) => storedKey(s, threadKey));
  const thread = useSession((s) => (key ? s.threads[key] : undefined));
  const hasHistory = useSession((s) => s.kind !== "teamspeak");
  const allFailed = useOutbox((o) => o.failed);
  const failed = useMemo(() => allFailed.filter((f) => f.thread === key), [allFailed, key]);
  const rows = useMemo(() => buildRows(thread?.items ?? []), [thread?.items]);
  const ref = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const restoreFrom = useRef<number | null>(null);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [showJump, setShowJump] = useState(false);
  const firstKey = thread?.items[0]?.key;
  const lastKey = thread?.items.at(-1)?.key;
  const isChannel = key?.startsWith("ch:") ?? false;
  const locked = isChannel && !!thread?.locked;
  const viewChannel = useSession((s) => s.viewChannel);
  const vc = useSession((s) => s.kind === "vc");
  const canManageMessages = usePermission("message_manage");
  const editing = useUi((s) => s.editing);

  // The editor belongs to this conversation.
  useEffect(() => () => useUi.getState().setEditing(null), []);

  const loadOlder = useCallback(async () => {
    const el = ref.current;
    if (!el || loadingOlder) return;
    restoreFrom.current = el.scrollHeight - el.scrollTop;
    setLoadingOlder(true);
    await controller.loadOlder();
    setLoadingOlder(false);
  }, [loadingOlder]);

  const onScroll = () => {
    const el = ref.current;
    if (!el) return;
    const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    stick.current = atBottom;
    setShowJump(!atBottom);
    if (el.scrollTop < 80 && isChannel && thread?.hasMore && thread.loaded && !loadingOlder) void loadOlder();
  };

  // The list remounts per conversation (see ChatArea), so start at the bottom.
  useLayoutEffect(() => {
    const el = ref.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    if (restoreFrom.current !== null) {
      // Older messages were prepended: keep what the user was looking at in place.
      el.scrollTop = el.scrollHeight - restoreFrom.current;
      restoreFrom.current = null;
    } else if (stick.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [firstKey, lastKey, rows.length, failed.length]);

  const jump = () => {
    const el = ref.current;
    if (el) el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  };

  const messageCount = thread?.items.filter((i) => i.kind === "msg").length ?? 0;

  return (
    <div className="relative min-h-0 flex-1">
      <div
        ref={ref}
        onScroll={onScroll}
        role="log"
        aria-live="polite"
        aria-label={t("chat.messages")}
        className="h-full overflow-y-auto overscroll-contain pt-2 pb-2"
      >
        {hasHistory && isChannel && thread && thread.loaded && !thread.hasMore && messageCount > 0 && (
          <div className="px-4 py-3 text-center text-xs text-subtle">{t("chat.beginning")}</div>
        )}
        {loadingOlder && (
          <div className="flex justify-center py-2 text-subtle">
            <Loader2 className="size-4 animate-spin" />
          </div>
        )}
        {isChannel && thread && !thread.loaded && (
          <div className="flex justify-center py-6 text-subtle">
            <Spinner />
          </div>
        )}
        {locked && viewChannel !== null && (
          <EmptyState icon={<Lock className="size-5" />} title={t("chat.lockedTitle")}>
            {t("chat.lockedHint")}
            <div className="mt-3">
              <Button variant="primary" onClick={() => useUi.getState().openDialog({ kind: "channelPassword", channel: viewChannel })}>
                {t("chat.lockedAction")}
              </Button>
            </div>
          </EmptyState>
        )}
        {!locked && rows.length === 0 && failed.length === 0 && thread?.loaded !== false && (
          <EmptyState icon={<MessageSquare className="size-5" />} title={t("chat.empty")}>
            {t(
              threadKey === "server"
                ? "chat.emptyServer"
                : threadKey === "channel"
                  ? hasHistory
                    ? "chat.emptyChannel"
                    : "chat.emptyChannelNoHistory"
                  : "chat.emptyDm",
            )}
          </EmptyState>
        )}
        {rows.map((row) => {
          if (row.type === "day")
            return (
              <div key={row.key} className="my-3 flex items-center gap-3 px-4" role="separator">
                <div className="h-px flex-1 bg-line" />
                <span className="text-xs font-medium text-subtle">{formatDay(row.at, lang, t)}</span>
                <div className="h-px flex-1 bg-line" />
              </div>
            );
          if (row.type === "sys") return <SysRow key={row.key} row={row} t={t} />;
          const m = row.item.msg;
          // Only channel messages (stored on the server) can be edited or deleted.
          const stored = vc && typeof m.target === "object" && "channel" in m.target;
          const own = m.author_uid === mineUid;
          return (
            <MessageRow
              key={row.key}
              row={row}
              mine={own}
              meUid={mineUid}
              lang={lang}
              compact={compact}
              canEdit={stored && own}
              canDelete={stored && (own || canManageMessages)}
              editing={editing === m.id}
            />
          );
        })}
        {failed.map((f) => (
          <div key={f.id} role="alert" className="mx-4 mt-2 flex gap-3 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2">
            <AlertCircle className="mt-0.5 size-4 shrink-0 text-danger" />
            <div className="min-w-0 flex-1">
              <div className="text-xs font-medium text-danger">
                {t("chat.notSent")}: {f.error}
              </div>
              <div className="mt-0.5 text-sm break-words whitespace-pre-wrap text-fg/80">{f.text}</div>
              <div className="mt-1.5 flex gap-2">
                <button
                  onClick={() => void controller.retrySend(f.id)}
                  className="t flex cursor-pointer items-center gap-1 rounded-md bg-danger px-2 py-1 text-xs font-medium text-white hover:brightness-110"
                >
                  <RotateCw className="size-3" /> {t("chat.retry")}
                </button>
                <button
                  onClick={() => useOutbox.getState().remove(f.id)}
                  className="t cursor-pointer rounded-md px-2 py-1 text-xs font-medium text-muted hover:bg-hover hover:text-fg"
                >
                  {t("chat.dismiss")}
                </button>
              </div>
            </div>
          </div>
        ))}
      </div>
      {showJump && (
        <button
          onClick={jump}
          className="anim-pop absolute right-4 bottom-3 flex cursor-pointer items-center gap-1.5 rounded-full border border-line-strong bg-raised px-3 py-1.5 text-xs font-medium text-fg shadow-pop hover:bg-hover"
        >
          <ArrowDown className="size-3.5" /> {t("chat.jump")}
        </button>
      )}
    </div>
  );
}

// ----------------------------------------------------------------- composer

const MAX_FILES = 10;

interface PendingFile {
  id: number;
  file: File;
  status: "uploading" | "done" | "error";
  progress: number;
  fileId?: string;
  error?: string;
  preview?: string;
  abort: AbortController;
}

let pendingId = 1;

function uploadErrorText(e: unknown, t: TFn): string {
  const limit = formatBytes(useSession.getState().server?.upload_limit ?? 0);
  if (e instanceof UploadError) {
    switch (e.kind) {
      case "too_large":
        return t("file.tooLarge", { limit });
      case "disabled":
      case "unsupported":
        return t("file.disabled");
      case "empty":
        return t("file.empty");
      case "forbidden":
        return t("file.forbidden");
      case "aborted":
        return t("file.aborted");
      case "network":
        return t("file.networkFailed");
    }
  }
  return e instanceof Error ? e.message : String(e);
}

/** Files waiting to be sent, shown above the text field with their upload progress. */
function PendingFiles({ files, onRemove }: { files: PendingFile[]; onRemove: (id: number) => void }) {
  const t = useT();
  if (files.length === 0) return null;
  return (
    <ul aria-label={t("file.pending")} className="flex flex-wrap gap-2 px-1 pt-1 pb-1.5">
      {files.map((f) => (
        <li
          key={f.id}
          data-pending={f.file.name}
          className={cn(
            "relative flex h-14 w-48 items-center gap-2 overflow-hidden rounded-lg border bg-surface pr-7 pl-1.5",
            f.status === "error" ? "border-danger/60" : "border-line-strong",
          )}
        >
          {f.preview ? (
            <img src={f.preview} alt="" className="size-11 shrink-0 rounded-md object-cover" />
          ) : (
            <span className="flex size-11 shrink-0 items-center justify-center rounded-md bg-accent-soft text-accent">
              <Paperclip className="size-4" />
            </span>
          )}
          <span className="min-w-0 flex-1">
            <span className="block truncate text-xs font-medium" title={f.file.name}>
              {f.file.name}
            </span>
            <span className={cn("block truncate text-[11px]", f.status === "error" ? "text-danger" : "text-subtle")} title={f.error}>
              {f.status === "error" ? f.error : f.status === "done" ? formatBytes(f.file.size) : `${Math.round(f.progress * 100)}%`}
            </span>
          </span>
          {f.status === "uploading" && (
            <div
              role="progressbar"
              aria-label={f.file.name}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(f.progress * 100)}
              className="absolute inset-x-0 bottom-0 h-0.5 bg-active"
            >
              <div className="h-full bg-accent transition-[width] duration-100" style={{ width: `${Math.round(f.progress * 100)}%` }} />
            </div>
          )}
          <button
            type="button"
            aria-label={t("file.remove", { name: f.file.name })}
            onClick={() => onRemove(f.id)}
            className="t absolute top-1 right-1 flex size-5 cursor-pointer items-center justify-center rounded text-subtle hover:bg-hover hover:text-fg"
          >
            <X className="size-3" />
          </button>
        </li>
      ))}
    </ul>
  );
}

function Composer({ threadKey, attachRef }: { threadKey: ThreadKey; attachRef: { current: ((files: File[]) => void) | null } }) {
  const t = useT();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [pendingByThread, setPendingByThread] = useState<Record<string, PendingFile[]>>({});
  const text = drafts[threadKey] ?? "";
  const pending = useMemo(() => pendingByThread[threadKey] ?? [], [pendingByThread, threadKey]);
  const ref = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const online = useSession((s) => s.phase === "online");
  const peerOnline = useSession((s) => (threadKey.startsWith("dm:") ? Object.values(s.clients).some((c) => c.uid === threadKey.slice(3)) : true));
  const peerName = useSession((s) => (threadKey.startsWith("dm:") ? s.threads[threadKey]?.peer?.name : undefined));
  const channelName = useSession((s) => (s.viewChannel === null ? "" : (s.channels[s.viewChannel]?.name ?? "")));
  // A password channel I have not entered: the server would refuse, and I cannot read it either.
  const locked = useSession((s) => threadKey === "channel" && s.viewChannel !== null && !!s.threads[`ch:${s.viewChannel}`]?.locked);
  const uploadsOn = useSession((s) => s.kind === "vc" && (s.server?.upload_limit ?? 0) > 0);
  const mayUpload = usePermission("file_upload");
  // Files travel with channel messages only.
  const canAttach = threadKey === "channel" && uploadsOn && mayUpload;
  const disabled = !online || !peerOnline || locked;
  const over = text.length > MAX_MESSAGE_LENGTH;
  const uploading = pending.some((f) => f.status === "uploading");
  const ready = pending.filter((f) => f.status === "done");
  const hasContent = !!text.trim() || ready.length > 0;

  const setText = (value: string) => setDrafts((d) => ({ ...d, [threadKey]: value }));
  const mention = useMentionAutocomplete(text, setText, ref);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
  }, [text, threadKey]);

  // Focus the composer when switching conversations on desktop.
  useEffect(() => {
    if (window.matchMedia("(pointer: fine)").matches) ref.current?.focus();
  }, [threadKey]);

  const patch = useCallback((key: string, id: number, change: Partial<PendingFile>) => {
    setPendingByThread((all) => ({ ...all, [key]: (all[key] ?? []).map((f) => (f.id === id ? { ...f, ...change } : f)) }));
  }, []);

  const addFiles = useCallback(
    (files: File[]) => {
      if (files.length === 0) return;
      if (!canAttach) {
        useUi.getState().toast("error", t(uploadsOn && mayUpload ? "file.channelOnly" : "file.disabled"));
        return;
      }
      const limit = useSession.getState().server?.upload_limit ?? 0;
      const room = MAX_FILES - (pendingByThread[threadKey]?.length ?? 0);
      if (files.length > room) useUi.getState().toast("error", t("file.tooMany", { max: MAX_FILES }));
      const key = threadKey;
      for (const file of files.slice(0, Math.max(0, room))) {
        const check = checkUpload(file.size, limit);
        if (!check.ok) {
          useUi.getState().toast("error", `${file.name}: ${uploadErrorText(new UploadError(check.reason), t)}`);
          continue;
        }
        const id = pendingId++;
        const abort = new AbortController();
        const preview = /^image\/(png|jpeg|gif|webp)$/.test(file.type) ? URL.createObjectURL(file) : undefined;
        setPendingByThread((all) => ({ ...all, [key]: [...(all[key] ?? []), { id, file, status: "uploading", progress: 0, preview, abort }] }));
        controller
          .uploadFile(file, { signal: abort.signal, onProgress: (p) => patch(key, id, { progress: p }) })
          .then((fileId) => patch(key, id, { status: "done", progress: 1, fileId }))
          .catch((e: unknown) => {
            if (e instanceof UploadError && e.kind === "aborted") return;
            patch(key, id, { status: "error", error: uploadErrorText(e, t) });
          });
      }
    },
    [canAttach, mayUpload, uploadsOn, pendingByThread, threadKey, patch, t],
  );

  // The chat area forwards dropped files here.
  useEffect(() => {
    attachRef.current = addFiles;
    return () => {
      attachRef.current = null;
    };
  }, [attachRef, addFiles]);

  const remove = (id: number) => {
    setPendingByThread((all) => {
      const list = all[threadKey] ?? [];
      const f = list.find((x) => x.id === id);
      f?.abort.abort();
      if (f?.preview) URL.revokeObjectURL(f.preview);
      return { ...all, [threadKey]: list.filter((x) => x.id !== id) };
    });
  };

  const send = async () => {
    const value = text.trim();
    if ((!value && ready.length === 0) || over || disabled || uploading) return;
    const mentions = mention.mentionsFor(value);
    const attachments = ready.map((f) => f.fileId!).filter(Boolean);
    // Clear right away so quick follow-ups are not lost. A failed send shows up
    // in the conversation with a Retry button (see MessageList).
    setDrafts((d) => ({ ...d, [threadKey]: "" }));
    setPendingByThread((all) => {
      // Failed uploads stay for another try; the sent ones go (their previews are no longer needed).
      for (const f of all[threadKey] ?? []) if (f.status === "done" && f.preview) URL.revokeObjectURL(f.preview);
      return { ...all, [threadKey]: (all[threadKey] ?? []).filter((f) => f.status !== "done") };
    });
    mention.reset();
    ref.current?.focus();
    await controller.sendChat(threadKey, value, { mentions, attachments });
  };

  const editLast = () => {
    const s = useSession.getState();
    const key = storedKey(s, threadKey);
    const items = (key && s.threads[key]?.items) || [];
    for (let i = items.length - 1; i >= 0; i--) {
      const item = items[i];
      if (item?.kind === "msg" && item.msg.author_uid === s.me?.uid && typeof item.msg.target === "object" && "channel" in item.msg.target) {
        useUi.getState().setEditing(item.msg.id);
        return true;
      }
    }
    return false;
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (mention.onKeyDown(e)) return;
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void send();
    } else if (e.key === "ArrowUp" && !text && pending.length === 0 && useSession.getState().kind === "vc" && threadKey === "channel") {
      if (editLast()) e.preventDefault();
    }
  };

  const onPaste = (e: ClipboardEvent<HTMLTextAreaElement>) => {
    const files = [...e.clipboardData.files];
    if (files.length === 0) return;
    e.preventDefault();
    addFiles(files);
  };

  const placeholder = locked
    ? t("chat.placeholderLocked")
    : !peerOnline
    ? t("chat.peerOfflineNamed", { name: peerName ?? "" })
    : threadKey === "server"
      ? t("chat.placeholderServer")
      : threadKey === "channel"
        ? t("chat.placeholderChannel", { name: channelName })
        : t("chat.placeholderDm", { name: peerName ?? "" });

  return (
    <div className="shrink-0 px-4 pt-1 pb-4 [padding-bottom:max(1rem,env(safe-area-inset-bottom))]">
      <div className="relative">
        {mention.menu}
        <div
          className={cn(
            "t rounded-xl border bg-side focus-within:border-accent focus-within:ring-2 focus-within:ring-accent/25",
            over ? "border-danger" : "border-line-strong",
          )}
        >
          <PendingFiles files={pending} onRemove={remove} />
          <div className="flex items-end gap-1 py-1.5 pr-1.5 pl-1.5">
            {canAttach && (
              <>
                <IconButton label={t("file.attach")} disabled={disabled} onClick={() => fileInput.current?.click()} className="self-end">
                  <Paperclip className="size-[18px]" />
                </IconButton>
                <input
                  ref={fileInput}
                  type="file"
                  multiple
                  hidden
                  aria-label={t("file.attachInput")}
                  onChange={(e) => {
                    addFiles([...(e.target.files ?? [])]);
                    e.target.value = "";
                  }}
                />
              </>
            )}
            <textarea
              ref={ref}
              rows={1}
              value={text}
              disabled={disabled}
              onChange={(e) => {
                setText(e.target.value);
                mention.sync(e.target);
              }}
              onBeforeInput={mention.beforeInput}
              onSelect={(e) => mention.sync(e.currentTarget)}
              onKeyDown={onKeyDown}
              onPaste={onPaste}
              placeholder={placeholder}
              aria-label={t("chat.compose")}
              className={cn(
                "max-h-40 min-h-7 flex-1 resize-none bg-transparent py-1 text-sm leading-5 text-fg outline-none placeholder:text-subtle disabled:opacity-60",
                canAttach ? "pl-1" : "pl-1.5",
              )}
            />
            {text.length > MAX_MESSAGE_LENGTH - 500 && (
              <span className={cn("self-center text-xs tabular-nums", over ? "text-danger" : "text-subtle")}>
                {text.length}/{MAX_MESSAGE_LENGTH}
              </span>
            )}
            <IconButton
              label={t("chat.send")}
              tone="accent"
              disabled={disabled || !hasContent || over || uploading}
              onClick={() => void send()}
              className="self-end"
            >
              <Send className="size-4" />
            </IconButton>
          </div>
        </div>
      </div>
    </div>
  );
}

// --------------------------------------------------------------------- tabs

function ThreadTabs() {
  const t = useT();
  const active = useSession((s) => s.activeThread);
  const channelName = useSession((s) => (s.viewChannel === null ? "" : (s.channels[s.viewChannel]?.name ?? "")));
  const channelUnread = useSession((s) => {
    const k = storedKey(s, "channel");
    return k ? (s.threads[k]?.unread ?? 0) : 0;
  });
  const channelMentionCount = useSession((s) => {
    const k = storedKey(s, "channel");
    return k ? (s.threads[k]?.mentions ?? 0) : 0;
  });
  const serverUnread = useSession((s) => s.threads["server"]?.unread ?? 0);
  const threads = useSession((s) => s.threads);
  const knownMembers = useSession((s) => s.members);
  const dispatch = useSession((s) => s.dispatch);
  const dms = useMemo(() => Object.entries(threads).filter(([k]) => k.startsWith("dm:")), [threads]);

  const tab = (key: ThreadKey, label: string, icon: React.ReactNode, unread: number, closable?: string, mentionCount = 0) => {
    const selected = active === key;
    return (
      <div
        key={key}
        className={cn(
          "t group relative flex h-8 shrink-0 items-center rounded-md text-sm",
          selected ? "bg-active text-fg" : "text-muted hover:bg-hover hover:text-fg",
        )}
      >
        <button
          role="tab"
          aria-selected={selected}
          onClick={() => dispatch({ type: "setActive", key })}
          className="flex h-full max-w-48 cursor-pointer items-center gap-1.5 rounded-md pr-3 pl-2.5"
          style={closable ? { paddingRight: 28 } : undefined}
        >
          {icon}
          <span className="truncate">{label}</span>
          <MentionBadge count={mentionCount} />
          <UnreadBadge count={unread} />
        </button>
        {closable && (
          <button
            aria-label={t("chat.closeDm")}
            onClick={() => dispatch({ type: "closeDm", uid: closable })}
            className="t absolute right-1 flex size-5 cursor-pointer items-center justify-center rounded text-subtle opacity-0 group-hover:opacity-100 hover:bg-hover hover:text-fg focus-visible:opacity-100"
          >
            <X className="size-3" />
          </button>
        )}
      </div>
    );
  };

  return (
    <div role="tablist" aria-label={t("chat.conversations")} className="flex shrink-0 items-center gap-1 overflow-x-auto border-b border-line px-3 py-1.5">
      {tab("channel", channelName || t("chat.channel"), <Hash className="size-3.5 shrink-0" />, channelUnread, undefined, channelMentionCount)}
      {tab("server", t("chat.server"), <Megaphone className="size-3.5 shrink-0" />, serverUnread)}
      {dms.map(([k, th]) => tab(k as ThreadKey, memberName(knownMembers, k.slice(3), th.peer?.name ?? "?"), <MessageSquare className="size-3.5 shrink-0" />, th.unread, k.slice(3)))}
    </div>
  );
}

// ------------------------------------------------------------------- header

function ChatHeader() {
  const t = useT();
  const lang = useLanguage();
  const isMobile = useIsMobile();
  const active = useSession((s) => s.activeThread);
  const setDrawer = useUi((s) => s.setDrawer);
  const server = useSession((s) => s.server);
  const phase = useSession((s) => s.phase);
  const channel = useSession((s) => (s.viewChannel === null ? undefined : s.channels[s.viewChannel]));
  // People talking in the channel whose chat is open.
  const userCount = useSession((s) => Object.values(s.clients).filter((c) => c.channel !== null && c.channel === s.viewChannel).length);
  const inVoice = useSession((s) => myChannelId(s) !== null);
  const members = useMembersPanel();
  const dmName = useSession((s) => (active.startsWith("dm:") ? memberName(s.members, active.slice(3), s.threads[active]?.peer?.name ?? "") : undefined));

  let title: string;
  let sub: string;
  let icon = <Hash className="size-5 text-subtle" />;
  if (active === "channel") {
    title = channel?.name ?? "";
    sub = channel?.topic || t("chat.noTopic");
  } else if (active === "server") {
    icon = <Megaphone className="size-5 text-subtle" />;
    title = t("chat.server");
    sub = t("chat.serverHint", { name: server?.name ?? "" });
  } else {
    icon = <MessageSquare className="size-5 text-subtle" />;
    title = dmName ?? "";
    sub = t("chat.dmHint");
  }

  return (
    <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-3 md:px-4">
      {isMobile && (
        <IconButton label={t("nav.channels")} onClick={() => setDrawer(true)} className="-ml-1">
          <Menu className="size-5" />
        </IconButton>
      )}
      {icon}
      <div className="min-w-0 flex-1">
        <h2 className="truncate text-[15px] leading-tight font-semibold">{title}</h2>
        <p className="truncate text-xs leading-tight text-muted" title={sub}>
          {sub}
        </p>
      </div>
      {active === "channel" && userCount > 0 && (
        <span className="hidden shrink-0 text-xs text-subtle sm:inline">{t(countKey(lang, "chat.users", userCount), { count: userCount })}</span>
      )}
      {isMobile && phase !== "idle" && inVoice && (
        <div className="flex items-center gap-1">
          <MicButton size="sm" />
          <DeafenButton size="sm" />
        </div>
      )}
      <IconButton
        label={t("chat.members")}
        active={members.open}
        onClick={members.toggle}
        className={cn(members.open && !members.isMobile && "bg-active text-fg")}
      >
        <Users className="size-[18px]" />
      </IconButton>
    </header>
  );
}

export function ChatArea() {
  const t = useT();
  const active = useSession((s) => s.activeThread);
  const phase = useSession((s) => s.phase);
  const storage = useSession((s) => storedKey(s, s.activeThread));
  const attachRef = useRef<((files: File[]) => void) | null>(null);
  const [dragging, setDragging] = useState(false);
  const depth = useRef(0);

  const hasFiles = (e: DragEvent) => [...e.dataTransfer.types].includes("Files");
  const onDragEnter = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    depth.current++;
    setDragging(true);
  };
  const onDragLeave = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    depth.current = Math.max(0, depth.current - 1);
    if (depth.current === 0) setDragging(false);
  };
  const onDrop = (e: DragEvent) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    depth.current = 0;
    setDragging(false);
    attachRef.current?.([...e.dataTransfer.files]);
  };

  return (
    <main
      className="relative flex min-w-0 flex-1 flex-col bg-surface"
      onDragEnter={onDragEnter}
      onDragOver={(e) => hasFiles(e) && e.preventDefault()}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      <ChatHeader />
      {phase === "reconnecting" && (
        <div role="status" className="flex items-center gap-2 bg-warn/15 px-4 py-1.5 text-xs font-medium text-warn">
          <Loader2 className="size-3.5 animate-spin" /> {t("chat.reconnecting")}
        </div>
      )}
      <ThreadTabs />
      <MessageList key={storage ?? active} threadKey={active} />
      <Composer threadKey={active} attachRef={attachRef} />
      {dragging && (
        <div
          aria-hidden
          className="pointer-events-none absolute inset-2 z-30 flex items-center justify-center rounded-xl border-2 border-dashed border-accent bg-surface/85 text-sm font-medium text-accent"
        >
          <Paperclip className="mr-2 size-5" /> {t("file.dropHint")}
        </div>
      )}
    </main>
  );
}
