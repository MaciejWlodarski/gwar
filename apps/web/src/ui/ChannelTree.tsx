import * as ContextMenu from "@radix-ui/react-context-menu";
import {
  ChevronRight,
  Hash,
  HeadphoneOff,
  Headphones,
  Loader2,
  Lock,
  LogIn,
  MessageSquare,
  MicOff,
  MoveRight,
  Pencil,
  Plus,
  Trash2,
  UserX,
  Volume2,
} from "lucide-react";
import { memo, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { useT } from "../i18n";
import { PLATFORM_LABEL } from "../lib/platform";
import { cn } from "../lib/cn";
import type { Client } from "../proto/Client";
import { controller } from "../state/controller";
import { channelUnread, myChannelId } from "../state/reducer";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { buildTree, descendantsCount, type ChannelNode } from "../state/tree";
import { UnreadBadge } from "./badges";
import { usePermission } from "./hooks";
import { Avatar, EmptyState, Slider, menuContent, menuItem, menuItemDanger, menuLabel, menuSeparator } from "./kit";

const CLIENT_MIME = "application/x-vc-client";

export function ChannelTree() {
  const t = useT();
  const channels = useSession((s) => s.channels);
  const clients = useSession((s) => s.clients);
  const compact = useSettings((s) => s.compact);
  const tree = useMemo(() => buildTree(Object.values(channels), Object.values(clients)), [channels, clients]);
  const ref = useRef<HTMLDivElement>(null);

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "Home" && e.key !== "End") return;
    const items = [...(ref.current?.querySelectorAll<HTMLElement>("[data-treeitem]") ?? [])];
    const index = items.indexOf((document.activeElement as HTMLElement | null)?.closest<HTMLElement>("[data-treeitem]") as HTMLElement);
    let next = index;
    if (e.key === "ArrowDown") next = Math.min(items.length - 1, index + 1);
    if (e.key === "ArrowUp") next = Math.max(0, index - 1);
    if (e.key === "Home") next = 0;
    if (e.key === "End") next = items.length - 1;
    items[next]?.focus();
    e.preventDefault();
  };

  if (tree.length === 0) {
    return <EmptyState icon={<Hash className="size-5" />} title={t("tree.empty")}>{t("tree.emptyHint")}</EmptyState>;
  }
  return (
    <div ref={ref} role="tree" aria-label={t("tree.label")} onKeyDown={onKeyDown} className={cn("flex flex-col px-2 py-2", compact ? "gap-0" : "gap-px")}>
      {tree.map((node) => (
        <ChannelBranch key={node.channel.id} node={node} compact={compact} />
      ))}
    </div>
  );
}

function ChannelBranch({ node, compact }: { node: ChannelNode; compact: boolean }) {
  const collapsed = useUi((s) => !!s.collapsed[node.channel.id]);
  return (
    <div role="group">
      <ChannelRow node={node} collapsed={collapsed} compact={compact} />
      {!collapsed && (
        <>
          {node.clients.map((c) => (
            <UserRow key={c.id} client={c} depth={node.depth} compact={compact} />
          ))}
          {node.children.map((child) => (
            <ChannelBranch key={child.channel.id} node={child} compact={compact} />
          ))}
        </>
      )}
    </div>
  );
}

const ChannelRow = memo(function ChannelRow({ node, collapsed, compact }: { node: ChannelNode; collapsed: boolean; compact: boolean }) {
  const t = useT();
  const { channel } = node;
  // `current`: the voice channel I am in. `selected`: the chat I have open.
  const current = useSession((s) => myChannelId(s) === channel.id);
  const selected = useSession((s) => s.activeThread === "channel" && s.viewChannel === channel.id);
  const unread = useSession((s) => channelUnread(s, channel.id));
  const isDefault = useSession((s) => s.server?.default_channel === channel.id);
  const canMove = usePermission("client_move");
  const canCreate = usePermission("channel_create");
  const canEdit = usePermission("channel_edit");
  const canDelete = usePermission("channel_delete");
  const joining = useUi((s) => s.joining === channel.id);
  const toggleCollapsed = useUi((s) => s.toggleCollapsed);
  const openDialog = useUi((s) => s.openDialog);
  const [dragOver, setDragOver] = useState(false);
  const expandable = node.children.length > 0 || node.clients.length > 0;
  const total = descendantsCount(node);
  const count = node.clients.length;

  const join = () => void controller.joinChannelInteractive(channel.id);
  const select = () => {
    controller.selectChannel(channel.id);
    useUi.getState().setDrawer(false);
  };

  const onKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.target !== e.currentTarget) return; // keys on the join button are its own
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (e.shiftKey) join();
      else select();
    } else if (e.key === "ArrowRight" && collapsed) {
      toggleCollapsed(channel.id);
    } else if (e.key === "ArrowLeft" && !collapsed && expandable) {
      toggleCollapsed(channel.id);
    }
  };

  return (
    <ContextMenu.Root>
      <ContextMenu.Trigger asChild>
        <div
          role="treeitem"
          aria-level={node.depth + 1}
          aria-expanded={expandable ? !collapsed : undefined}
          aria-selected={selected}
          aria-current={current ? "true" : undefined}
          data-treeitem
          tabIndex={0}
          onClick={select}
          onDoubleClick={join}
          onKeyDown={onKey}
          onDragOver={(e) => {
            if (canMove && e.dataTransfer.types.includes(CLIENT_MIME)) {
              e.preventDefault();
              e.dataTransfer.dropEffect = "move";
              setDragOver(true);
            }
          }}
          onDragLeave={() => setDragOver(false)}
          onDrop={(e) => {
            setDragOver(false);
            const id = Number(e.dataTransfer.getData(CLIENT_MIME));
            if (canMove && id) {
              e.preventDefault();
              void controller.moveClient(id, channel.id);
            }
          }}
          style={{ paddingLeft: 4 + node.depth * 16 }}
          className={cn(
            "t group flex cursor-pointer items-center gap-1 rounded-md pr-2 text-sm",
            compact ? "h-7" : "h-8",
            selected ? "bg-accent-soft font-medium text-fg" : unread > 0 ? "font-semibold text-fg hover:bg-hover" : "text-muted hover:bg-hover hover:text-fg",
            dragOver && "bg-accent/25 ring-1 ring-accent",
          )}
        >
          <span
            role="presentation"
            onClick={(e) => {
              e.stopPropagation();
              if (expandable) toggleCollapsed(channel.id);
            }}
            className={cn("flex size-5 shrink-0 items-center justify-center rounded text-subtle hover:text-fg", !expandable && "invisible")}
          >
            <ChevronRight className={cn("size-3.5 transition-transform duration-150", !collapsed && "rotate-90")} />
          </span>
          {joining ? (
            <Loader2 aria-label={t("chat.joiningShort")} className="size-4 shrink-0 animate-spin text-accent" />
          ) : current ? (
            <Volume2 aria-hidden className="size-4 shrink-0 text-accent" />
          ) : (
            <Hash className={cn("size-4 shrink-0", selected ? "text-accent" : "text-subtle")} />
          )}
          <span className="min-w-0 flex-1 truncate">{channel.name}</span>
          {current && <span className="sr-only">({t("tree.inVoice")})</span>}
          {channel.has_password && <Lock aria-label={t("tree.locked")} className="size-3.5 shrink-0 text-subtle" />}
          {(count > 0 || channel.max_clients) && !collapsed && (
            <span className="shrink-0 text-xs tabular-nums text-subtle">
              {channel.max_clients ? `${count}/${channel.max_clients}` : count}
            </span>
          )}
          {collapsed && total > 0 && <span className="shrink-0 rounded-full bg-hover px-1.5 text-xs tabular-nums text-muted">{total}</span>}
          <UnreadBadge count={unread} />
          {!current && !joining && (
            <button
              type="button"
              aria-label={t("tree.joinVoiceNamed", { name: channel.name })}
              title={t("tree.joinVoice")}
              onClick={(e) => {
                e.stopPropagation();
                join();
              }}
              onDoubleClick={(e) => e.stopPropagation()}
              className="t flex size-6 w-0 shrink-0 cursor-pointer items-center justify-center overflow-hidden rounded text-muted opacity-0 group-focus-within:w-6 group-focus-within:opacity-100 group-hover:w-6 group-hover:opacity-100 hover:bg-active hover:text-fg pointer-coarse:w-6 pointer-coarse:opacity-100"
            >
              <Headphones className="size-3.5" />
            </button>
          )}
        </div>
      </ContextMenu.Trigger>
      <ContextMenu.Portal>
        <ContextMenu.Content className={menuContent}>
          <ContextMenu.Label className={menuLabel}>{channel.name}</ContextMenu.Label>
          <ContextMenu.Item className={menuItem} onSelect={select}>
            <MessageSquare className="size-4" /> {t("tree.openChat")}
          </ContextMenu.Item>
          <ContextMenu.Item className={menuItem} disabled={current} onSelect={join}>
            <LogIn className="size-4" /> {t("tree.joinVoice")}
          </ContextMenu.Item>
          {(canCreate || canEdit || canDelete) && <ContextMenu.Separator className={menuSeparator} />}
          {canCreate && (
            <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "channelEdit", mode: "create", parent: channel.id })}>
              <Plus className="size-4" /> {t("tree.createSub")}
            </ContextMenu.Item>
          )}
          {canEdit && (
            <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "channelEdit", mode: "edit", channel: channel.id })}>
              <Pencil className="size-4" /> {t("tree.edit")}
            </ContextMenu.Item>
          )}
          {canDelete && (
            <ContextMenu.Item
              className={cn(menuItem, menuItemDanger)}
              disabled={isDefault}
              onSelect={() =>
                openDialog({
                  kind: "confirm",
                  title: t("tree.deleteTitle", { name: channel.name }),
                  body: t("tree.deleteBody"),
                  confirmLabel: t("common.delete"),
                  danger: true,
                  onConfirm: () => void controller.attempt(controller.deleteChannel(channel.id)),
                })
              }
            >
              <Trash2 className="size-4" /> {t("common.delete")}
            </ContextMenu.Item>
          )}
        </ContextMenu.Content>
      </ContextMenu.Portal>
    </ContextMenu.Root>
  );
});


function UserRow({ client, depth, compact }: { client: Client; depth: number; compact: boolean }) {
  const t = useT();
  const isMe = useSession((s) => s.me?.session === client.id);
  const myChannel = useSession(myChannelId);
  const canMove = usePermission("client_move");
  const canKick = usePermission("client_kick");
  const openDm = () => {
    if (isMe) return;
    useSession.getState().dispatch({ type: "openDm", uid: client.uid, name: client.nickname });
    useUi.getState().setDrawer(false);
  };
  const avatarSize = compact ? 18 : 22;
  const silenced = client.muted || client.deafened;

  return (
    <ContextMenu.Root>
      <ContextMenu.Trigger asChild>
        <div
          role="treeitem"
          aria-level={depth + 2}
          aria-selected={false}
          data-treeitem
          tabIndex={-1}
          draggable={canMove}
          onDragStart={(e) => {
            e.dataTransfer.setData(CLIENT_MIME, String(client.id));
            e.dataTransfer.effectAllowed = "move";
          }}
          onDoubleClick={openDm}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              openDm();
            }
          }}
          style={{ paddingLeft: 4 + (depth + 1) * 16 + 20 }}
          className={cn(
            "t flex items-center gap-2 rounded-md pr-2 text-sm text-fg hover:bg-hover",
            compact ? "h-6" : "h-7",
          )}
        >
          <span className={cn("relative rounded-full", client.talking && "talking-ring")}>
            <Avatar name={client.nickname} seed={client.uid} size={avatarSize} />
          </span>
          <span className={cn("min-w-0 truncate", silenced && "text-muted", isMe && "font-medium")}>{client.nickname}</span>
          {client.away !== null && (
            <span className="min-w-0 max-w-[40%] truncate text-xs text-subtle" title={client.away}>
              {client.away || t("tree.away")}
            </span>
          )}
          <span className="ml-auto flex shrink-0 items-center gap-1.5">
            {client.muted && !client.deafened && <MicOff aria-label={t("tree.muted")} className="size-3.5 text-danger" />}
            {client.deafened && <HeadphoneOff aria-label={t("tree.deafened")} className="size-3.5 text-danger" />}
            {!compact && client.platform !== "web" && (
              <span className="rounded bg-hover px-1 text-[10px] font-medium leading-4 text-subtle">{PLATFORM_LABEL[client.platform]}</span>
            )}
          </span>
        </div>
      </ContextMenu.Trigger>
      <ContextMenu.Portal>
        <ContextMenu.Content className={cn(menuContent, "w-60")}>
          <ContextMenu.Label className={menuLabel}>
            {client.nickname} · {PLATFORM_LABEL[client.platform]}
          </ContextMenu.Label>
          {!isMe && (
            <>
              <VolumeControl uid={client.uid} />
              <ContextMenu.Separator className={menuSeparator} />
              <ContextMenu.Item className={menuItem} onSelect={openDm}>
                <MessageSquare className="size-4" /> {t("tree.pm")}
              </ContextMenu.Item>
            </>
          )}
          {canMove && myChannel !== null && client.channel !== myChannel && (
            <ContextMenu.Item className={menuItem} onSelect={() => void controller.moveClient(client.id, myChannel)}>
              <MoveRight className="size-4" /> {t("tree.moveHere")}
            </ContextMenu.Item>
          )}
          {canKick && !isMe && (
            <ContextMenu.Item
              className={cn(menuItem, menuItemDanger)}
              onSelect={() =>
                useUi.getState().openDialog({
                  kind: "confirm",
                  title: t("tree.kickTitle", { name: client.nickname }),
                  body: t("tree.kickBody"),
                  confirmLabel: t("tree.kick"),
                  danger: true,
                  onConfirm: () => void controller.kickClient(client.id),
                })
              }
            >
              <UserX className="size-4" /> {t("tree.kick")}
            </ContextMenu.Item>
          )}
          {isMe && <div className="px-2 py-1.5 text-xs text-subtle">{t("tree.you")}</div>}
        </ContextMenu.Content>
      </ContextMenu.Portal>
    </ContextMenu.Root>
  );
}

function VolumeControl({ uid }: { uid: string }): ReactNode {
  const t = useT();
  const volume = useSettings((s) => s.userVolumes[uid] ?? 1);
  const set = useSettings((s) => s.setUserVolume);
  return (
    <div className="px-2 py-2" onPointerDown={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
      <div className="mb-1 flex items-center justify-between text-xs text-muted">
        <span className="flex items-center gap-1.5">
          <Volume2 className="size-3.5" /> {t("tree.volume")}
        </span>
        <button
          className="t cursor-pointer rounded px-1 tabular-nums hover:bg-hover hover:text-fg"
          onClick={() => set(uid, 1)}
          title={t("tree.volumeReset")}
        >
          {Math.round(volume * 100)}%
        </button>
      </div>
      <Slider value={Math.round(volume * 100)} min={0} max={200} step={5} onChange={(v) => set(uid, v / 100)} label={t("tree.volume")} />
    </div>
  );
}
