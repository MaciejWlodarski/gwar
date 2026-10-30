import * as ContextMenu from "@radix-ui/react-context-menu";
import { Check, ChevronRight, Folder, Plus } from "lucide-react";
import type { CSSProperties } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useBookmarks, useServerGroups, useSettings, type Bookmark } from "../state/settings";
import { useAccount } from "../state/account";
import { useUi } from "../state/stores";
import { TeamSpeakBadge } from "./badges";
import { Avatar, Tooltip, menuContent, menuItem, menuItemDanger, menuSeparator, panelCard } from "./kit";
import { railLayout, useBookmarkOpener, type ShownGroup } from "./nav";

/** Moving a server between groups, or into a new one: the same entries in its context menu and its group's menu. */
function GroupSubmenu({ server }: { server: Bookmark }) {
  const t = useT();
  const groups = useServerGroups();
  const openDialog = useUi((s) => s.openDialog);
  const current = groups.find((g) => g.servers.includes(server.id));
  return (
    <ContextMenu.Sub>
      <ContextMenu.SubTrigger className={menuItem}>
        <span className="flex-1">{t("group.move")}</span>
        <ChevronRight className="size-4 text-subtle" />
      </ContextMenu.SubTrigger>
      <ContextMenu.Portal>
        <ContextMenu.SubContent className={menuContent} sideOffset={4}>
          {groups.map((g) => (
            <ContextMenu.Item key={g.id} className={menuItem} onSelect={() => useSettings.getState().moveServerToGroup(server.id, g.id)}>
              <span className="size-2.5 shrink-0 rounded-[3px]" style={{ background: g.color }} />
              <span className="flex-1 truncate">{g.name}</span>
              {current?.id === g.id && <Check className="size-4 text-accent" />}
            </ContextMenu.Item>
          ))}
          {groups.length > 0 && <ContextMenu.Separator className={menuSeparator} />}
          <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "serverGroup", server: server.id })}>
            {t("group.new")}
          </ContextMenu.Item>
          {current && (
            <ContextMenu.Item className={menuItem} onSelect={() => useSettings.getState().moveServerToGroup(server.id, null)}>
              {t("group.leave")}
            </ContextMenu.Item>
          )}
        </ContextMenu.SubContent>
      </ContextMenu.Portal>
    </ContextMenu.Sub>
  );
}

/** How strongly an unfolded group tints the column behind its servers. */
const WASH_PERCENT = 10;

/** `wash`: the colour of the group it sits in, whose faint wash its badge's cut-out then matches. */
function ServerButton({ b, card, wash }: { b: Bookmark; card?: boolean; wash?: string }) {
  const t = useT();
  const openDialog = useUi((s) => s.openDialog);
  const { isActive, open } = useBookmarkOpener();
  const active = isActive(b);
  return (
    <ContextMenu.Root>
      <ContextMenu.Trigger asChild>
        <div className="relative flex w-full justify-center">
          <Tooltip label={`${b.name}${b.kind === "teamspeak" ? ` · ${t("badge.teamspeak")}` : ""}`} side="right">
            <button
              onClick={() => open(b)}
              aria-label={b.name}
              aria-current={active ? "true" : undefined}
              className={cn(
                "t cursor-pointer overflow-hidden transition-[border-radius] duration-150",
                // An outline, not a ring: its gap shows whatever is behind it, the column or a group's wash.
                active ? "rounded-xl outline-2 outline-offset-2 outline-accent" : "rounded-[22px] hover:rounded-xl",
              )}
            >
              <Avatar name={b.name} seed={b.address} size={44} square className="!rounded-none" />
            </button>
          </Tooltip>
          {b.kind === "teamspeak" && (
            <TeamSpeakBadge
              short
              className={cn("pointer-events-none absolute right-0 -bottom-0.5 ring-2", card ? "ring-side" : "ring-rail")}
              style={wash ? ({ "--tw-ring-color": `color-mix(in srgb, ${wash} ${WASH_PERCENT}%, var(--${card ? "side" : "rail"}))` } as CSSProperties) : undefined}
            />
          )}
        </div>
      </ContextMenu.Trigger>
      <ContextMenu.Portal>
        <ContextMenu.Content className={menuContent}>
          <ContextMenu.Item className={menuItem} onSelect={() => open(b)}>
            {t("rail.connect")}
          </ContextMenu.Item>
          <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "addServer", editId: b.id })}>
            {t("rail.edit")}
          </ContextMenu.Item>
          <GroupSubmenu server={b} />
          <ContextMenu.Separator className={menuSeparator} />
          <ContextMenu.Item className={cn(menuItem, menuItemDanger)} onSelect={() => useSettings.getState().removeBookmark(b.id)}>
            {t("rail.remove")}
          </ContextMenu.Item>
        </ContextMenu.Content>
      </ContextMenu.Portal>
    </ContextMenu.Root>
  );
}

/**
 * A group, folded like a Discord folder: one button that is a tile of four of its servers while folded
 * and a folder icon over them while unfolded, inside a faint wash of the group's colour. Its name is only
 * in the tooltip; its context menu edits it.
 */
function RailGroup({ group, servers, card }: ShownGroup & { card?: boolean }) {
  const t = useT();
  const openDialog = useUi((s) => s.openDialog);
  const { isActive } = useBookmarkOpener();
  const toggle = () => useSettings.getState().updateServerGroup(group.id, { collapsed: !group.collapsed });
  // While folded the tile stands in for the open server, so it takes the server's ring.
  const active = group.collapsed && servers.some(isActive);
  return (
    <div
      role="group"
      aria-label={group.name}
      className={cn("flex shrink-0 flex-col items-center gap-2", !group.collapsed && "-mx-1.5 w-14 rounded-[22px] py-1.5")}
      style={group.collapsed ? undefined : { background: `color-mix(in srgb, ${group.color} ${WASH_PERCENT}%, transparent)`, boxShadow: `inset 0 0 0 1px ${group.color}2e` }}
    >
      <ContextMenu.Root>
        <ContextMenu.Trigger asChild>
          <div className="flex w-full justify-center">
            <Tooltip label={group.name} side="right">
              <button
                onClick={toggle}
                aria-label={group.name}
                aria-expanded={!group.collapsed}
                aria-current={active ? "true" : undefined}
                className={cn(
                  "t size-11 shrink-0 cursor-pointer",
                  group.collapsed ? "grid grid-cols-2 place-items-center rounded-2xl p-1.5" : "flex items-center justify-center rounded-[22px] hover:rounded-xl",
                  active && "outline-2 outline-offset-2 outline-accent",
                )}
                style={group.collapsed ? { background: `${group.color}26` } : undefined}
              >
                {group.collapsed ? (
                  servers.slice(0, 4).map((b) => <Avatar key={b.id} name={b.name} seed={b.address} size={14} square className="!rounded-[4px]" />)
                ) : (
                  <Folder className="size-5" fill="currentColor" style={{ color: group.color }} />
                )}
              </button>
            </Tooltip>
          </div>
        </ContextMenu.Trigger>
        <ContextMenu.Portal>
          <ContextMenu.Content className={menuContent}>
            <ContextMenu.Item className={menuItem} onSelect={toggle}>
              {group.collapsed ? t("group.expand") : t("group.collapse")}
            </ContextMenu.Item>
            <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "serverGroup", id: group.id })}>
              {t("group.edit")}
            </ContextMenu.Item>
            <ContextMenu.Separator className={menuSeparator} />
            <ContextMenu.Item className={cn(menuItem, menuItemDanger)} onSelect={() => useSettings.getState().removeServerGroup(group.id)}>
              {t("group.delete")}
            </ContextMenu.Item>
          </ContextMenu.Content>
        </ContextMenu.Portal>
      </ContextMenu.Root>
      {!group.collapsed && servers.map((b) => <ServerButton key={b.id} b={b} card={card} wash={group.color} />)}
    </div>
  );
}

/** The saved servers: a card of its own beside the others on desktop, a plain column beside the channels on a phone. */
export function ServerRail({ card }: { card?: boolean }) {
  const t = useT();
  const bookmarks = useBookmarks();
  const groups = useServerGroups();
  const openDialog = useUi((s) => s.openDialog);
  const layout = railLayout(bookmarks, groups);
  const named = useSettings((s) => s.onboarding?.completed === true);
  const signedIn = useAccount((s) => s.account !== null);

  return (
    <nav aria-label={t("rail.label")} className={cn("flex w-16 shrink-0 flex-col items-center", card ? cn(panelCard, "bg-side py-1.5") : "h-full bg-rail py-3")}>
      <div className="flex min-h-0 w-full flex-1 flex-col items-center gap-2 overflow-y-auto overflow-x-hidden px-2.5 py-1.5">
        {layout.loose.map((b) => (
          <ServerButton key={b.id} b={b} card={card} />
        ))}
        {layout.groups.length > 0 && layout.loose.length > 0 && <div aria-hidden className="my-0.5 h-px w-8 shrink-0 bg-line-strong" />}
        {layout.groups.map((g) => (
          <RailGroup key={g.group.id} group={g.group} servers={g.servers} card={card} />
        ))}
        {(named || signedIn) && <Tooltip label={t("rail.add")} side="right">
          <button
            aria-label={t("rail.add")}
            onClick={() => openDialog({ kind: "addServer" })}
            className="t flex size-11 shrink-0 cursor-pointer items-center justify-center rounded-[22px] border border-dashed border-line-strong text-muted hover:rounded-xl hover:border-accent hover:text-accent"
          >
            <Plus className="size-5" />
          </button>
        </Tooltip>}
      </div>
    </nav>
  );
}
