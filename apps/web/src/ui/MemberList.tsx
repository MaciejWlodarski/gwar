import * as ContextMenu from "@radix-ui/react-context-menu";
import { MessageSquare, Shield, UserRound } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { useLanguage, useT } from "../i18n";
import { cn } from "../lib/cn";
import { isTeamSpeak, PLATFORM_LABEL } from "../lib/platform";
import { formatRelative } from "../lib/time";
import { useSession, useUi } from "../state/stores";
import { Avatar, menuContent, menuItem, menuLabel, Tooltip } from "./kit";
import { ModerationItems } from "./ModerationMenu";
import { ConnectBadge, IdentityLine } from "./badges";
import { groupMembers, type MemberEntry } from "./members";

function MemberRow({ entry, mine, now }: { entry: MemberEntry; mine: boolean; now: number }) {
  const t = useT();
  const lang = useLanguage();
  const offline = !entry.online;
  const lastSeen = offline
    ? entry.lastSeen > 0
      ? t("members.lastSeen", { when: formatRelative(entry.lastSeen, now, lang) })
      : t("members.lastSeenUnknown")
    : undefined;
  const open = () => {
    if (mine) return;
    useSession.getState().dispatch({ type: "openDm", uid: entry.uid, name: entry.nickname });
    useUi.getState().setMembersDrawer(false);
  };
  return (
    <li>
      <ContextMenu.Root>
        <Tooltip label={
          <div className="flex flex-col gap-1">
            <span>{entry.nickname}</span>
            <IdentityLine connect={entry.connect} tag={entry.tag} />
            {lastSeen && <span>{lastSeen}</span>}
          </div>
        }>
          <ContextMenu.Trigger asChild>
            <button
              type="button"
              onClick={open}
              title={lastSeen ?? (entry.channel ? `${entry.nickname} · ${entry.channel}` : undefined)}
              aria-label={mine ? undefined : t("members.openChat", { name: entry.nickname })}
              className={cn(
                "t flex h-9 w-full items-center gap-2 rounded-md px-2 text-left text-sm hover:bg-hover",
                mine ? "cursor-default" : "cursor-pointer",
              )}
            >
              <span className={cn("shrink-0", offline && "opacity-50")}>
                <Avatar name={entry.nickname} seed={entry.uid} size={24} />
              </span>
              <span className="min-w-0 flex-1 leading-tight">
                <span
                  className={cn("block truncate", offline ? "text-subtle" : "text-fg", mine && "font-medium")}
                  style={entry.color ? { color: entry.color } : undefined}
                >
                  {entry.nickname}
                </span>
                {entry.channel && <span className="block truncate text-[11px] text-subtle">{entry.channel}</span>}
              </span>
              <ConnectBadge handle={entry.connect} />
              {entry.admin && <Shield aria-label={t("members.admin")} className="size-3.5 shrink-0 text-accent" />}
              {entry.platform && isTeamSpeak(entry.platform) && (
                <span className="shrink-0 rounded bg-hover px-1 text-[10px] leading-4 font-medium text-subtle">{PLATFORM_LABEL[entry.platform]}</span>
              )}
            </button>
          </ContextMenu.Trigger>
        </Tooltip>
        <ContextMenu.Portal>
          <ContextMenu.Content className={cn(menuContent, "w-56")}>
            <ContextMenu.Label className={menuLabel}>{entry.nickname}</ContextMenu.Label>
            <ContextMenu.Item className={menuItem} onSelect={() => useUi.getState().openDialog({ kind: "memberProfile", uid: entry.uid, fallback: entry.nickname })}>
              <UserRound className="size-4" /> {t("member.viewProfile")}
            </ContextMenu.Item>
            {!mine && (
              <ContextMenu.Item className={menuItem} onSelect={open}>
                <MessageSquare className="size-4" /> {t("tree.pm")}
              </ContextMenu.Item>
            )}
            <ModerationItems person={{ uid: entry.uid, nickname: entry.nickname, session: entry.session ?? undefined }} groups={entry.groups} />
            {mine && <div className="px-2 py-1.5 text-xs text-subtle">{t("tree.you")}</div>}
          </ContextMenu.Content>
        </ContextMenu.Portal>
      </ContextMenu.Root>
    </li>
  );
}

function Section({ title, entries, mineUid, now }: { title: string; entries: MemberEntry[]; mineUid: string | undefined; now: number }) {
  if (entries.length === 0) return null;
  return (
    <section aria-label={title} className="px-2 pt-3">
      <h3 className="px-2 pb-1 text-[11px] font-semibold tracking-wide text-subtle uppercase">
        {title} <span className="tabular-nums">— {entries.length}</span>
      </h3>
      <ul>
        {entries.map((e) => (
          <MemberRow key={e.uid} entry={e} mine={e.uid === mineUid} now={now} />
        ))}
      </ul>
    </section>
  );
}

/** Who is on the server: in voice, online, and everyone else who was ever here. */
export function MemberList({ className }: { className?: string }) {
  const t = useT();
  const clients = useSession((s) => s.clients);
  const members = useSession((s) => s.members);
  const groups = useSession((s) => s.groups);
  const channels = useSession((s) => s.channels);
  const mineUid = useSession((s) => s.me?.uid);
  // "Last seen" labels age while the panel stays open.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(id);
  }, []);
  const { voice, online, offline } = useMemo(
    () => groupMembers(Object.values(clients), Object.values(members), groups, (id) => channels[id]?.name ?? null),
    [clients, members, groups, channels],
  );

  return (
    <aside aria-label={t("chat.members")} className={cn("flex h-full w-60 shrink-0 flex-col border-l border-line bg-side", className)}>
      <h2 className="flex h-12 shrink-0 items-center border-b border-line px-4 text-[15px] font-semibold">{t("members.title")}</h2>
      <div className="min-h-0 flex-1 overflow-y-auto pb-3">
        <Section title={t("members.voice")} entries={voice} mineUid={mineUid} now={now} />
        <Section title={t("members.online")} entries={online} mineUid={mineUid} now={now} />
        <Section title={t("members.offline")} entries={offline} mineUid={mineUid} now={now} />
      </div>
    </aside>
  );
}
