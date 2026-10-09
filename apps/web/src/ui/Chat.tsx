import { AlertCircle, ArrowDown, Hash, Loader2, Lock, Menu, Megaphone, MessageSquare, RotateCw, Send, Users, X } from "lucide-react";
import { Fragment, memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { countKey, useLanguage, useT, type TFn } from "../i18n";
import { cn } from "../lib/cn";
import { buildRows, type Row } from "../lib/chat";
import { useIsMobile } from "../lib/media";
import { tokenizeText } from "../lib/text";
import { MAX_MESSAGE_LENGTH } from "../net/protocol";
import { controller } from "../state/controller";
import { myChannelId, storedKey } from "../state/reducer";
import { useOutbox, useSession, useUi } from "../state/stores";
import type { ThreadKey } from "../state/types";
import { UnreadBadge } from "./badges";
import { useMembersPanel } from "./members";
import { DeafenButton, MicButton } from "./VoicePanel";
import { Avatar, Button, EmptyState, IconButton, Spinner } from "./kit";

// -------------------------------------------------------------------- text

function MessageText({ text }: { text: string }) {
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
        ) : (
          <Fragment key={i}>{tok.value}</Fragment>
        ),
      )}
    </>
  );
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
  lang,
  compact,
}: {
  row: Extract<Row, { type: "msg" }>;
  mine: boolean;
  lang: string;
  compact: boolean;
}) {
  const { msg, at } = row.item;
  const time = formatTime(at, lang);
  const full = new Date(at).toLocaleString(lang);
  return (
    <div className={cn("group relative flex gap-3 px-4 hover:bg-hover/60", row.first ? (compact ? "pt-2" : "pt-3") : "pt-px")}>
      <div className="w-9 shrink-0 pt-0.5">
        {row.first ? (
          <Avatar name={msg.author_name} seed={msg.author_uid} size={36} />
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
            <span className={cn("text-sm font-semibold", mine && "text-accent")}>{msg.author_name}</span>
            <time dateTime={new Date(at).toISOString()} title={full} className="text-xs text-subtle">
              {time}
            </time>
          </div>
        )}
        <div className="text-sm leading-[1.5] break-words whitespace-pre-wrap text-fg">
          <MessageText text={msg.text} />
        </div>
      </div>
    </div>
  );
});

function SysRow({ row, t }: { row: Extract<Row, { type: "sys" }>; t: TFn }) {
  const { text } = row.item;
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
      {t(text.key, text.params)}
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
          return <MessageRow key={row.key} row={row} mine={row.item.msg.author_uid === mineUid} lang={lang} compact={compact} />;
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

function Composer({ threadKey }: { threadKey: ThreadKey }) {
  const t = useT();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const text = drafts[threadKey] ?? "";
  const ref = useRef<HTMLTextAreaElement>(null);
  const online = useSession((s) => s.phase === "online");
  const peerOnline = useSession((s) => (threadKey.startsWith("dm:") ? Object.values(s.clients).some((c) => c.uid === threadKey.slice(3)) : true));
  const peerName = useSession((s) => (threadKey.startsWith("dm:") ? s.threads[threadKey]?.peer?.name : undefined));
  const channelName = useSession((s) => (s.viewChannel === null ? "" : (s.channels[s.viewChannel]?.name ?? "")));
  // A password channel I have not entered: the server would refuse, and I cannot read it either.
  const locked = useSession((s) => threadKey === "channel" && s.viewChannel !== null && !!s.threads[`ch:${s.viewChannel}`]?.locked);
  const disabled = !online || !peerOnline || locked;
  const over = text.length > MAX_MESSAGE_LENGTH;

  const setText = (value: string) => setDrafts((d) => ({ ...d, [threadKey]: value }));

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

  const send = async () => {
    const value = text.trim();
    if (!value || over || disabled) return;
    // Clear right away so quick follow-ups are not lost. A failed send shows up
    // in the conversation with a Retry button (see MessageList).
    setDrafts((d) => ({ ...d, [threadKey]: "" }));
    ref.current?.focus();
    await controller.sendChat(threadKey, value);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      void send();
    }
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
      <div
        className={cn(
          "t flex items-end gap-2 rounded-xl border bg-side py-1.5 pr-1.5 pl-3 focus-within:border-accent focus-within:ring-2 focus-within:ring-accent/25",
          over ? "border-danger" : "border-line-strong",
        )}
      >
        <textarea
          ref={ref}
          rows={1}
          value={text}
          disabled={disabled}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={placeholder}
          aria-label={t("chat.compose")}
          className="max-h-40 min-h-7 flex-1 resize-none bg-transparent py-1 text-sm leading-5 text-fg outline-none placeholder:text-subtle disabled:opacity-60"
        />
        {text.length > MAX_MESSAGE_LENGTH - 500 && (
          <span className={cn("self-center text-xs tabular-nums", over ? "text-danger" : "text-subtle")}>
            {text.length}/{MAX_MESSAGE_LENGTH}
          </span>
        )}
        <IconButton
          label={t("chat.send")}
          tone="accent"
          disabled={disabled || !text.trim() || over}
          onClick={() => void send()}
          className="self-end"
        >
          <Send className="size-4" />
        </IconButton>
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
  const serverUnread = useSession((s) => s.threads["server"]?.unread ?? 0);
  const threads = useSession((s) => s.threads);
  const dispatch = useSession((s) => s.dispatch);
  const dms = useMemo(() => Object.entries(threads).filter(([k]) => k.startsWith("dm:")), [threads]);

  const tab = (key: ThreadKey, label: string, icon: React.ReactNode, unread: number, closable?: string) => {
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
      {tab("channel", channelName || t("chat.channel"), <Hash className="size-3.5 shrink-0" />, channelUnread)}
      {tab("server", t("chat.server"), <Megaphone className="size-3.5 shrink-0" />, serverUnread)}
      {dms.map(([k, th]) => tab(k as ThreadKey, th.peer?.name ?? "?", <MessageSquare className="size-3.5 shrink-0" />, th.unread, k.slice(3)))}
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
  const dmName = useSession((s) => (active.startsWith("dm:") ? s.threads[active]?.peer?.name : undefined));

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
  return (
    <main className="flex min-w-0 flex-1 flex-col bg-surface">
      <ChatHeader />
      {phase === "reconnecting" && (
        <div role="status" className="flex items-center gap-2 bg-warn/15 px-4 py-1.5 text-xs font-medium text-warn">
          <Loader2 className="size-3.5 animate-spin" /> {t("chat.reconnecting")}
        </div>
      )}
      <ThreadTabs />
      <MessageList key={storage ?? active} threadKey={active} />
      <Composer threadKey={active} />
    </main>
  );
}
