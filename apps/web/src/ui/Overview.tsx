import { Hash, LayoutDashboard, LogIn, Megaphone, Menu, MessageSquare, UserPlus, Volume2 } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useLanguage, useT } from "../i18n";
import { cn } from "../lib/cn";
import { useIsMobile } from "../lib/media";
import { messageTarget } from "../lib/chat";
import { formatRelative } from "../lib/time";
import { fullyVerified } from "../net/message-signature";
import { messageStorage } from "../net/message-storage";
import { MessageVerificationCache } from "../net/message-verification-cache";
import type { Client } from "../proto/Client";
import { controller } from "../state/controller";
import { channelUnread, myChannelId } from "../state/reducer";
import { useSession, useUi } from "../state/stores";
import type { ChatItem } from "../state/types";
import { TeamSpeakBadge, TsIcon, UnreadBadge } from "./badges";
import { useChannelPermission, usePermission } from "./hooks";
import { Avatar, Button, IconButton, TalkingAvatar, panelCard } from "./kit";
import { newsworthy, stillHeld } from "./nav";
import { LiveBadge } from "./Screen";

/** How many people the overview names before "and N more". */
const ONLINE_SHOWN = 12;
/** The latest server chat messages it repeats. */
const NEWS_SHOWN = 3;

function Card({ icon, title, count, children, className }: { icon: ReactNode; title: string; count?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={cn("flex min-w-0 flex-col rounded-xl border border-line bg-side p-4", className)}>
      <h3 className="mb-3 flex items-center gap-2 text-xs font-semibold tracking-wide text-subtle uppercase">
        {icon}
        <span className="flex-1">{title}</span>
        {count !== undefined && <span className="font-normal normal-case tracking-normal">{count}</span>}
      </h3>
      {children}
    </section>
  );
}

function Quiet({ children }: { children: ReactNode }) {
  return <p className="text-sm text-subtle">{children}</p>;
}

/** A voice channel with people in it: who, whether someone shares, and a way in. */
function VoiceRoom({ id, name, people, mine }: { id: number; name: string; people: Client[]; mine: boolean }) {
  const t = useT();
  const nicknames = useSession((s) => s.members);
  // Per channel, overrides included (docs/roles.md).
  const canJoin = useChannelPermission(id, "connect");
  const sharer = people.find((c) => c.sharing);
  return (
    <li className="flex items-center gap-3 rounded-lg px-2 py-2 hover:bg-hover">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2 text-sm font-medium">
          <Volume2 className="size-4 shrink-0 text-subtle" />
          <span className="truncate">{name}</span>
          {sharer && <LiveBadge client={sharer} name={nicknames[sharer.uid]?.nickname ?? sharer.nickname} />}
        </div>
        <div className="mt-1.5 flex flex-wrap items-center gap-1">
          {people.slice(0, 8).map((c) => (
            <TalkingAvatar
              key={c.id}
              talking={c.talking}
              name={nicknames[c.uid]?.nickname ?? c.nickname}
              seed={c.uid}
              uid={c.uid}
              size={24}
            />
          ))}
          {people.length > 8 && <span className="ml-1 text-xs text-subtle">+{people.length - 8}</span>}
        </div>
      </div>
      {mine ? (
        <span className="shrink-0 text-xs font-medium text-accent">{t("overview.youAreHere")}</span>
      ) : (
        canJoin && (
          <Button size="sm" onClick={() => void controller.joinChannelInteractive(id)} aria-label={t("tree.joinVoiceNamed", { name })}>
            <LogIn className="size-3.5" /> {t("overview.join")}
          </Button>
        )
      )}
    </li>
  );
}

type News = Extract<ChatItem, { kind: "msg" }>;

/**
 * The latest server messages fit to repeat. On Gwar each is verified again here, as the chat does, and
 * again every minute and whenever the revocation feed changes: the flag cached when it arrived does not
 * learn of a revocation since. TeamSpeak messages carry no signatures and are shown like in the chat.
 */
function useNews(items: ChatItem[] | undefined, teamspeak: boolean, serverKey: string): News[] {
  // Another server starts with an empty cache.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const verifier = useMemo(() => new MessageVerificationCache(), [serverKey]);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    const timer = setInterval(() => setTick((n) => n + 1), 60_000);
    const unsubscribe = messageStorage.subscribe(() => setTick((n) => n + 1));
    return () => {
      clearInterval(timer);
      unsubscribe();
    };
  }, []);
  const candidates = useMemo(() => (items ?? []).filter((i) => newsworthy(i, teamspeak)), [items, teamspeak]);
  const [checked, setChecked] = useState<{ verifier: MessageVerificationCache; items: News[] }>({ verifier, items: [] });
  useEffect(() => {
    if (teamspeak) return;
    let active = true;
    void (async () => {
      const uid = await controller.messageUid;
      if (tick) await verifier.refresh();
      const picked: News[] = [];
      for (let i = candidates.length - 1; i >= 0 && picked.length < NEWS_SHOWN; i--) {
        const item = candidates[i]!;
        const ctx = { serverKey, target: messageTarget("server", item.msg.author_uid, uid), sealed: item.sealed ? { group: item.sealed.group, epoch: item.sealed.epoch } : undefined };
        if (fullyVerified(await verifier.verify(item.msg, ctx))) picked.push(item);
      }
      if (active) setChecked({ verifier, items: picked });
    })().catch(() => {});
    return () => {
      active = false;
    };
  }, [candidates, verifier, serverKey, tick, teamspeak]);
  if (teamspeak) return candidates.slice(-NEWS_SHOWN).reverse();
  // Until a check finishes, what it last let through shows only while the thread still holds it unchanged:
  // an edit or a deletion takes it away at once.
  return checked.verifier === verifier ? stillHeld(checked.items, candidates) : [];
}

function newsText(item: News, t: ReturnType<typeof useT>): string {
  if (item.msg.text) return item.msg.text;
  return item.msg.attachments?.length ? t("overview.attachment") : "";
}


/**
 * The server at a glance, in place of the chat: who talks in which room (with a
 * way in), who is online, the latest server messages and the unread channels.
 * Only what the client already knows; nothing extra is asked of the server.
 */
export function Overview({ card }: { card?: boolean }) {
  const t = useT();
  const lang = useLanguage();
  const isMobile = useIsMobile();
  const setDrawer = useUi((s) => s.setDrawer);
  const openDialog = useUi((s) => s.openDialog);
  const server = useSession((s) => s.server);
  const kind = useSession((s) => s.kind);
  const address = useSession((s) => s.address);
  const channels = useSession((s) => s.channels);
  const clients = useSession((s) => s.clients);
  const members = useSession((s) => s.members);
  const threads = useSession((s) => s.threads);
  const myChannel = useSession(myChannelId);
  const dispatch = useSession((s) => s.dispatch);
  const canInvite = usePermission("create_invites") && kind === "gwar";

  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);

  const nickname = (c: Client) => members[c.uid]?.nickname ?? c.nickname;
  const online = useMemo(
    () => Object.values(clients).sort((a, b) => (members[a.uid]?.nickname ?? a.nickname).localeCompare(members[b.uid]?.nickname ?? b.nickname)),
    [clients, members],
  );

  // Rooms by how many people are in them, the busiest first.
  const rooms = useMemo(() => {
    const byChannel = new Map<number, Client[]>();
    for (const c of Object.values(clients)) {
      if (c.channel === null || !channels[c.channel]) continue;
      byChannel.set(c.channel, [...(byChannel.get(c.channel) ?? []), c]);
    }
    return [...byChannel.entries()]
      .map(([id, people]) => ({ id, name: channels[id]?.name ?? "", people }))
      .sort((a, b) => b.people.length - a.people.length || a.name.localeCompare(b.name));
  }, [clients, channels]);

  const unread = useMemo(
    () =>
      Object.values(channels)
        .map((ch) => ({ id: ch.id, name: ch.name, count: channelUnread({ threads }, ch.id) }))
        .filter((ch) => ch.count > 0)
        .sort((a, b) => b.count - a.count),
    [channels, threads],
  );
  const dms = Object.entries(threads).filter(([k, th]) => k.startsWith("dm:") && th.unread > 0);
  const serverThread = threads["server"];
  const news = useNews(serverThread?.items, kind === "teamspeak", controller.messageServerKey);

  // The hero's way in: the busiest room, or the default channel when nobody talks.
  const target = rooms.find((r) => r.id !== myChannel) ?? null;
  const fallback = server && channels[server.default_channel] && server.default_channel !== myChannel ? server.default_channel : null;
  const joinId = myChannel === null ? (target?.id ?? fallback) : null;
  const canJoin = useChannelPermission(joinId, "connect");

  return (
    <main className={cn("relative flex min-w-0 flex-1 flex-col bg-surface", card && panelCard)}>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-3 md:px-4">
        {isMobile && (
          <IconButton label={t("nav.channels")} onClick={() => setDrawer(true)} className="-ml-1">
            <Menu className="size-5" />
          </IconButton>
        )}
        <LayoutDashboard className="size-5 text-subtle" />
        <div className="min-w-0 flex-1">
          <h2 className="truncate text-[15px] leading-tight font-semibold">{t("overview.title")}</h2>
          <p className="truncate text-xs leading-tight text-muted">{server?.name}</p>
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto flex w-full max-w-5xl flex-col gap-4 p-4 md:p-6">
          <section className="rounded-2xl border border-line bg-[linear-gradient(135deg,var(--accent-soft),transparent_60%)] p-5 md:p-6">
            <div className="flex items-center gap-3">
              <Avatar name={server?.name ?? "?"} seed={address} size={48} square />
              <div className="min-w-0">
                <h1 className="flex items-center gap-2 text-xl font-semibold tracking-tight">
                  {kind === "teamspeak" && <TsIcon token={server?.icon} />}
                  <span className="truncate">{server?.name}</span>
                  {kind === "teamspeak" && <TeamSpeakBadge />}
                </h1>
                <p className="text-sm text-muted">
                  {t("overview.onlineCount", { count: String(online.length) })} · {t("overview.voiceCount", { count: String(rooms.reduce((n, r) => n + r.people.length, 0)) })}
                </p>
              </div>
            </div>
            {(joinId !== null || canInvite) && (
              <div className="mt-4 flex flex-wrap gap-2">
                {joinId !== null && canJoin && (
                  <Button variant="primary" onClick={() => void controller.joinChannelInteractive(joinId)}>
                    <Volume2 className="size-4" /> {t("overview.joinNamed", { name: channels[joinId]?.name ?? "" })}
                  </Button>
                )}
                {canInvite && (
                  <Button onClick={() => openDialog({ kind: "invites" })}>
                    <UserPlus className="size-4" /> {t("server.invite")}
                  </Button>
                )}
              </div>
            )}
          </section>

          <div className="grid gap-4 lg:grid-cols-2">
            <Card icon={<Volume2 className="size-4" />} title={t("overview.voiceRooms")} count={rooms.length || undefined}>
              {rooms.length === 0 ? (
                <Quiet>{t("overview.nobodyTalks")}</Quiet>
              ) : (
                <ul className="-mx-2 flex flex-col">
                  {rooms.map((r) => (
                    <VoiceRoom key={r.id} id={r.id} name={r.name} people={r.people} mine={r.id === myChannel} />
                  ))}
                </ul>
              )}
            </Card>

            <Card icon={<span className="size-2 rounded-full bg-ok" />} title={t("overview.online")} count={online.length}>
              <ul className="-mx-2 flex flex-col">
                {online.slice(0, ONLINE_SHOWN).map((c) => (
                  <li key={c.id} className="flex items-center gap-2.5 rounded-lg px-2 py-1.5">
                    <Avatar name={nickname(c)} seed={c.uid} uid={c.uid} size={28} />
                    <div className="min-w-0 flex-1">
                      <div className="truncate text-sm">{nickname(c)}</div>
                      <div className="truncate text-xs text-subtle">
                        {c.channel !== null && channels[c.channel] ? t("overview.inRoom", { name: channels[c.channel]?.name ?? "" }) : t("overview.notInVoice")}
                      </div>
                    </div>
                  </li>
                ))}
              </ul>
              {online.length > ONLINE_SHOWN && <Quiet>{t("overview.more", { count: String(online.length - ONLINE_SHOWN) })}</Quiet>}
            </Card>

            {serverThread && (
              <Card icon={<Megaphone className="size-4" />} title={t("chat.server")}>
                {news.length === 0 ? (
                  <Quiet>{t("overview.noNews")}</Quiet>
                ) : (
                  <ul className="-mx-2 flex flex-col">
                    {news.map((item) => (
                      <li key={item.key}>
                        <button
                          onClick={() => dispatch({ type: "setActive", key: "server" })}
                          className="t flex w-full cursor-pointer flex-col rounded-lg px-2 py-1.5 text-left hover:bg-hover"
                        >
                          <span className="flex items-baseline gap-2 text-sm">
                            <span className="truncate font-medium">{members[item.msg.author_uid]?.nickname ?? item.msg.author_name}</span>
                            <span className="shrink-0 text-xs text-subtle">{formatRelative(item.at, now, lang)}</span>
                          </span>
                          <span className="line-clamp-2 text-sm text-muted">{newsText(item, t)}</span>
                        </button>
                      </li>
                    ))}
                  </ul>
                )}
              </Card>
            )}

            <Card icon={<Hash className="size-4" />} title={t("overview.unread")} count={unread.length + dms.length || undefined}>
              {unread.length + dms.length === 0 ? (
                <Quiet>{t("overview.allRead")}</Quiet>
              ) : (
                <ul className="-mx-2 flex flex-col">
                  {unread.map((ch) => (
                    <li key={ch.id}>
                      <button
                        onClick={() => controller.selectChannel(ch.id)}
                        className="t flex h-8 w-full cursor-pointer items-center gap-2 rounded-lg px-2 text-left text-sm hover:bg-hover"
                      >
                        <Hash className="size-4 shrink-0 text-subtle" />
                        <span className="min-w-0 flex-1 truncate">{ch.name}</span>
                        <UnreadBadge count={ch.count} />
                      </button>
                    </li>
                  ))}
                  {dms.map(([k, th]) => (
                    <li key={k}>
                      <button
                        onClick={() => dispatch({ type: "setActive", key: k as `dm:${string}` })}
                        className="t flex h-8 w-full cursor-pointer items-center gap-2 rounded-lg px-2 text-left text-sm hover:bg-hover"
                      >
                        <MessageSquare className="size-4 shrink-0 text-subtle" />
                        <span className="min-w-0 flex-1 truncate">{members[k.slice(3)]?.nickname ?? th.peer?.name ?? "?"}</span>
                        <UnreadBadge count={th.unread} />
                      </button>
                    </li>
                  ))}
                </ul>
              )}
            </Card>
          </div>
        </div>
      </div>
    </main>
  );
}
