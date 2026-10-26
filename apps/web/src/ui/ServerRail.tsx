import * as ContextMenu from "@radix-ui/react-context-menu";
import * as Dropdown from "@radix-ui/react-dropdown-menu";
import { Check, ChevronRight, Plus } from "lucide-react";
import { useT, useLanguage, countKey } from "../i18n";
import { cn } from "../lib/cn";
import { useBookmarks, useSettings, type Bookmark, type ServerGroup } from "../state/settings";
import { useUi } from "../state/stores";
import { TeamSpeakBadge } from "./badges";
import { Avatar, Tooltip, menuContent, menuItem, menuItemDanger, menuSeparator, panelCard } from "./kit";
import { railLayout, useBookmarkOpener, type ShownGroup } from "./nav";

/** Moving a server between groups, or into a new one: the same entries in its context menu and its group's menu. */
function GroupSubmenu({ server }: { server: Bookmark }) {
  const t = useT();
  const groups = useSettings((s) => s.serverGroups);
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

function ServerButton({ b, card }: { b: Bookmark; card?: boolean }) {
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
                "t cursor-pointer overflow-hidden transition-[border-radius,box-shadow] duration-150",
                active ? cn("rounded-xl ring-2 ring-accent ring-offset-2", card ? "ring-offset-side" : "ring-offset-rail") : "rounded-[22px] hover:rounded-xl",
              )}
            >
              <Avatar name={b.name} seed={b.address} size={44} square className="!rounded-none" />
            </button>
          </Tooltip>
          {b.kind === "teamspeak" && (
            <TeamSpeakBadge short className={cn("pointer-events-none absolute right-0 -bottom-0.5 ring-2", card ? "ring-side" : "ring-rail")} />
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

/** The group's coloured name above its servers: folds or unfolds it, and edits it from its context menu. */
function GroupLabel({ group }: { group: ServerGroup }) {
  const t = useT();
  const openDialog = useUi((s) => s.openDialog);
  const toggle = () => useSettings.getState().updateServerGroup(group.id, { collapsed: !group.collapsed });
  return (
    <ContextMenu.Root>
      <ContextMenu.Trigger asChild>
        <button
          onClick={toggle}
          aria-expanded={!group.collapsed}
          title={`${group.name} · ${group.collapsed ? t("group.expand") : t("group.collapse")}`}
          // The column's full width (past its padding), so short names fit unclipped.
          className="t -mx-2.5 w-16 cursor-pointer truncate px-1 text-center text-[9.5px] leading-4 font-bold tracking-[0.02em] uppercase opacity-90 hover:opacity-100"
          style={{ color: group.color }}
        >
          {group.name}
        </button>
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
  );
}

/**
 * A folded group: one tile with up to four of its servers, which opens a menu of them beside the column
 * (the column does not move). Ringed in the accent while the open server is one of them.
 */
function GroupTile({ group, servers, card }: ShownGroup & { card?: boolean }) {
  const t = useT();
  const lang = useLanguage();
  const openDialog = useUi((s) => s.openDialog);
  const { isActive, open } = useBookmarkOpener();
  const active = servers.some(isActive);
  return (
    <Dropdown.Root>
      <Tooltip label={group.name} side="right">
        <Dropdown.Trigger asChild>
          <button
            aria-label={t("group.tile", { name: group.name })}
            aria-current={active ? "true" : undefined}
            className={cn(
              "t grid size-11 shrink-0 cursor-pointer grid-cols-2 gap-0.5 rounded-xl border p-1 transition-shadow",
              active && cn("ring-2 ring-accent ring-offset-2", card ? "ring-offset-side" : "ring-offset-rail"),
            )}
            style={{ background: `${group.color}22`, borderColor: `${group.color}55` }}
          >
            {servers.slice(0, 4).map((b) => (
              <Avatar key={b.id} name={b.name} seed={b.address} size={16} square className="!rounded-[4px]" />
            ))}
          </button>
        </Dropdown.Trigger>
      </Tooltip>
      <Dropdown.Portal>
        <Dropdown.Content side="right" align="start" sideOffset={10} className={cn(menuContent, "w-64 p-1.5")}>
          <div className="flex items-center gap-2 px-2 pt-1 pb-2 text-sm font-semibold">
            <span className="size-2.5 shrink-0 rounded-[3px]" style={{ background: group.color }} />
            <span className="flex-1 truncate">{group.name}</span>
            <span className="text-xs font-normal text-subtle">{t(countKey(lang, "group.count", servers.length), { count: servers.length })}</span>
          </div>
          {servers.map((b) => (
            <Dropdown.Item key={b.id} className={cn(menuItem, "h-9")} onSelect={() => open(b)}>
              <Avatar name={b.name} seed={b.address} size={24} square />
              <span className="flex-1 truncate font-medium">{b.name}</span>
              {b.kind === "teamspeak" && <TeamSpeakBadge short />}
              {isActive(b) && <span className="text-xs text-accent">{t("group.current")}</span>}
            </Dropdown.Item>
          ))}
          <div className="mt-1.5 flex gap-1.5 border-t border-line px-0.5 pt-2">
            <Dropdown.Item
              className={cn(menuItem, "flex-1 justify-center whitespace-nowrap bg-hover text-xs text-muted")}
              onSelect={() => useSettings.getState().updateServerGroup(group.id, { collapsed: false })}
            >
              {t("group.expand")}
            </Dropdown.Item>
            <Dropdown.Item className={cn(menuItem, "flex-1 justify-center whitespace-nowrap bg-hover text-xs text-muted")} onSelect={() => openDialog({ kind: "serverGroup", id: group.id })}>
              {t("group.edit")}
            </Dropdown.Item>
          </div>
        </Dropdown.Content>
      </Dropdown.Portal>
    </Dropdown.Root>
  );
}

function RailGroup({ group, servers, card }: ShownGroup & { card?: boolean }) {
  return (
    <div role="group" aria-label={group.name} className="relative flex w-full flex-col items-center gap-2">
      <GroupLabel group={group} />
      {group.collapsed ? (
        <GroupTile group={group} servers={servers} card={card} />
      ) : (
        <>
          {/* The bracket: a coloured line along the unfolded group's servers. */}
          <span aria-hidden className="absolute top-6 bottom-0 -left-1.5 w-[3px] rounded-full" style={{ background: group.color }} />
          {servers.map((b) => (
            <ServerButton key={b.id} b={b} card={card} />
          ))}
        </>
      )}
    </div>
  );
}

/** The saved servers: a card of its own beside the others on desktop, a plain column in the mobile drawer. */
export function ServerRail({ card }: { card?: boolean }) {
  const t = useT();
  const bookmarks = useBookmarks();
  const groups = useSettings((s) => s.serverGroups);
  const openDialog = useUi((s) => s.openDialog);
  const layout = railLayout(bookmarks, groups);

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
        <Tooltip label={t("rail.add")} side="right">
          <button
            aria-label={t("rail.add")}
            onClick={() => openDialog({ kind: "addServer" })}
            className="t flex size-11 shrink-0 cursor-pointer items-center justify-center rounded-[22px] border border-dashed border-line-strong text-muted hover:rounded-xl hover:border-accent hover:text-accent"
          >
            <Plus className="size-5" />
          </button>
        </Tooltip>
      </div>
    </nav>
  );
}
