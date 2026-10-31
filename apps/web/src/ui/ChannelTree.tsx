import { NotificationBadge } from "./NotificationSettings";
import * as ContextMenu from "@radix-ui/react-context-menu";
import {
  Ban,
  ArrowDown,
  ArrowUp,
  ChevronRight,
  EyeOff,
  Info,
  Hash,
  HeadphoneOff,
  Headphones,
  House,
  Loader2,
  Lock,
  LogIn,
  MessageSquare,
  MicOff,
  MoveRight,
  Pencil,
  Plus,
  ShieldCheck,
  Trash2,
  Users,
  Volume2,
} from "lucide-react";
import { memo, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { useT } from "../i18n";
import { PLATFORM_LABEL } from "../lib/platform";
import { cn } from "../lib/cn";
import type { Client } from "../proto/Client";
import { controller } from "../state/controller";
import { channelMentions, channelUnread, myChannelId } from "../state/reducer";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { buildTree, descendantsCount, isFull, type ChannelNode } from "../state/tree";
import { fillText, parseSpacer } from "../lib/spacer";
import { MentionBadge, TsGroupBadges, TsIcon, UnreadBadge } from "./badges";
import { useTsImage } from "./ts-images";
import { useChannelPermission, useGroupsColor, useMyWho, useWhoOf } from "./hooks";
import { canActOn } from "../lib/permissions";
import { CLIENT_MIME, clientDragData, readClientDrag } from "../lib/client-drag";
import { ModerationItems } from "./ModerationMenu";
import { LiveBadge } from "./Screen";
import { EmptyState, TalkingAvatar, Slider, menuContent, menuItem, menuItemDanger, menuLabel, menuSeparator } from "./kit";

export function ChannelTree() {
  const t = useT();
  const channels = useSession((s) => s.channels);
  const clients = useSession((s) => s.clients);
  const compact = useSettings((s) => s.compact);
  const members = useSession((s) => s.members);
  const tree = useMemo(() => buildTree(Object.values(channels), Object.values(clients).map((c) => ({ ...c, nickname: members[c.uid]?.nickname ?? c.nickname }))), [channels, clients, members]);
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
  const chatShown = useUi((s) => !s.overview);
  const selected = useSession((s) => s.activeThread === "channel" && s.viewChannel === channel.id) && chatShown;
  const unread = useSession((s) => channelUnread(s, channel.id));
  const mentions = useSession((s) => channelMentions(s, channel.id));
  const isDefault = useSession((s) => s.server?.default_channel === channel.id);
  // Per channel, overrides included (docs/roles.md).
  const canMove = useChannelPermission(channel.id, "move_members");
  const canManage = useChannelPermission(channel.id, "manage_channels");
  const canCreate = canManage;
  const canEdit = canManage;
  const canDelete = canManage;
  const canJoin = useChannelPermission(channel.id, "connect");
  const onGwar = useSession((s) => s.kind === "gwar");
  const joining = useUi((s) => s.joining === channel.id);
  const toggleCollapsed = useUi((s) => s.toggleCollapsed);
  const openDialog = useUi((s) => s.openDialog);
  const [dragOver, setDragOver] = useState(false);
  const expandable = node.children.length > 0 || node.clients.length > 0;
  const total = descendantsCount(node);
  const count = node.clients.length;
  const full = isFull(node);
  const spacer = onGwar ? null : parseSpacer(channel.name, node.depth === 0);
  const familyMax = channel.max_family_clients ?? null;
  // TeamSpeak sorts siblings by hand: where this one stands among them.
  const place = useSession((s) => {
    if (s.kind !== "teamspeak") return null;
    const siblings = Object.values(s.channels).filter((c) => c.parent === channel.parent);
    const before = siblings.filter((c) => c.position < channel.position || (c.position === channel.position && c.id < channel.id)).length;
    return before === siblings.length - 1 ? (before === 0 ? "only" : "last") : before === 0 ? "first" : "middle";
  });

  const join = () => {
    if (!canJoin) return;
    // Joining opens the channel's chat beside the tree, but a phone stays on the tree, by its voice panel.
    const { pane } = useUi.getState();
    void controller.joinChannelInteractive(channel.id);
    useUi.getState().setPane(pane);
  };
  const select = () => {
    controller.selectChannel(channel.id);
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
          data-entry={selected || undefined}
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
            const drag = readClientDrag(e.dataTransfer.getData(CLIENT_MIME));
            if (canMove && drag) {
              e.preventDefault();
              void controller.moveClient(drag.id, drag.uid, channel.id);
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
          {/* Like TeamSpeak's channel bubbles: whether and how I can get in. */}
          {joining ? (
            <Loader2 aria-label={t("chat.joiningShort")} className="size-4 shrink-0 animate-spin text-accent" />
          ) : current ? (
            <Volume2 aria-hidden className="size-4 shrink-0 text-accent" />
          ) : spacer ? null : full ? (
            <Ban aria-label={t("tree.full")} className="size-4 shrink-0 text-danger" />
          ) : channel.has_password ? (
            <Lock aria-label={t("tree.locked")} className="size-4 shrink-0 text-warn" />
          ) : (
            <Hash className={cn("size-4 shrink-0", selected ? "text-accent" : "text-subtle")} />
          )}
          {spacer ? (
            <span
              aria-hidden={spacer.align === "fill" || undefined}
              className={cn(
                "min-w-0 flex-1 overflow-hidden whitespace-pre text-subtle",
                spacer.align === "center" && "text-center",
                spacer.align === "right" && "text-right",
              )}
            >
              {spacer.align === "fill" ? fillText(spacer.text) : spacer.text}
            </span>
          ) : (
            <span className="min-w-0 flex-1 truncate" title={channel.topic || undefined}>{channel.name}</span>
          )}
          {!spacer && !onGwar && <TsIcon token={channel.icon} />}
          {current && <span className="sr-only">({t("tree.inVoice")})</span>}
          {current && channel.has_password && <Lock aria-label={t("tree.locked")} className="size-3.5 shrink-0 text-warn" />}
          {isDefault && !spacer && <House aria-label={t("tree.default")} className="size-3.5 shrink-0 text-subtle" />}
          {channel.private && <EyeOff aria-label={t("tree.private")} className="size-3.5 shrink-0 text-subtle" />}
          {(count > 0 || (channel.max_clients !== null && !spacer)) && !collapsed && (
            <span
              className={cn("shrink-0 text-xs tabular-nums", full ? "font-medium text-danger" : "text-subtle")}
              title={channel.max_clients !== null ? t(full ? "tree.full" : "tree.limit", { max: channel.max_clients }) : undefined}
            >
              {channel.max_clients !== null ? `${count}/${channel.max_clients}` : count}
            </span>
          )}
          {familyMax !== null && !spacer && (
            <span
              className={cn("flex shrink-0 items-center gap-0.5 text-xs tabular-nums", node.familyFull ? "font-medium text-danger" : "text-subtle")}
              title={t("tree.familyLimit", { count: total, max: familyMax })}
            >
              <Users aria-hidden className="size-3" />
              {`${total}/${familyMax}`}
            </span>
          )}
          {collapsed && total > 0 && <span className="shrink-0 rounded-full bg-hover px-1.5 text-xs tabular-nums text-muted">{total}</span>}
          <button type="button" aria-label={t("notify.channelSettings", { name: channel.name })} className="flex size-6 shrink-0 cursor-pointer items-center justify-center rounded hover:bg-active" onClick={e => { e.stopPropagation(); openDialog({ kind: "notifications", channel: channel.id }); }} onDoubleClick={e => e.stopPropagation()}>
            <NotificationBadge channel={channel.id} always />
          </button>
          <MentionBadge count={mentions} />
          <UnreadBadge count={unread} />
          {!current && !joining && canJoin && (
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
          <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "notifications", channel: channel.id })}>
            <NotificationBadge channel={channel.id} always /> {t("notify.scopeTitle")}
          </ContextMenu.Item>
          <ContextMenu.Item className={menuItem} onSelect={select}>
            <MessageSquare className="size-4" /> {t("tree.openChat")}
          </ContextMenu.Item>
          <ContextMenu.Item className={menuItem} disabled={current || !canJoin} onSelect={join}>
            <LogIn className="size-4" /> {t("tree.joinVoice")}
          </ContextMenu.Item>
          {!onGwar && (
            <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "channelInfo", channel: channel.id })}>
              <Info className="size-4" /> {t("tree.info")}
            </ContextMenu.Item>
          )}
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
          {canEdit && place && (
            <>
              <ContextMenu.Item className={menuItem} disabled={place === "first" || place === "only"} onSelect={() => void controller.moveChannelBy(channel.id, -1)}>
                <ArrowUp className="size-4" /> {t("tree.moveUp")}
              </ContextMenu.Item>
              <ContextMenu.Item className={menuItem} disabled={place === "last" || place === "only"} onSelect={() => void controller.moveChannelBy(channel.id, 1)}>
                <ArrowDown className="size-4" /> {t("tree.moveDown")}
              </ContextMenu.Item>
            </>
          )}
          {canEdit && onGwar && (
            <ContextMenu.Item className={menuItem} onSelect={() => openDialog({ kind: "channelPermissions", channel: channel.id })}>
              <ShieldCheck className="size-4" /> {t("chperm.open")}
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


function UserRow({ client: sessionClient, depth, compact }: { client: Client; depth: number; compact: boolean }) {
  const t = useT();
  const member = useSession((s) => s.members[sessionClient.uid]);
  const client = { ...sessionClient, nickname: member?.nickname ?? sessionClient.nickname };
  const isMe = useSession((s) => s.me?.uid === client.uid);
  const myChannel = useSession(myChannelId);
  const mayMove = useChannelPermission(myChannel, "move_members");
  const me = useMyWho();
  const target = useWhoOf(client.uid, client.groups);
  const groups = useSession((s) => s.groups);
  const onGwar = useSession((s) => s.kind === "gwar");
  const avatar = useTsImage(onGwar ? null : sessionClient.avatar);
  // Myself, or someone ranked below me (docs/roles.md); a TeamSpeak server decides that itself.
  const canMove = mayMove && (isMe || !onGwar || canActOn(me, target, groups));
  const color = useGroupsColor(client.groups);
  const openDm = () => {
    if (isMe) return;
    useSession.getState().dispatch({ type: "openDm", uid: client.uid, name: client.nickname });
  };
  const avatarSize = compact ? 18 : 22; // plus the ring: the row height
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
            e.dataTransfer.setData(CLIENT_MIME, clientDragData({ id: client.id, uid: client.uid }));
            e.dataTransfer.effectAllowed = "move";
          }}
          onDoubleClick={openDm}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              openDm();
            }
          }}
          style={{ paddingLeft: 4 + (depth + 1) * 16 + 17 }}
          className={cn(
            "t flex items-center gap-1.5 rounded-md pr-2 text-sm text-fg hover:bg-hover",
            compact ? "h-6" : "h-7",
          )}
        >
          <TalkingAvatar name={client.nickname} seed={client.uid} image={avatar} size={avatarSize} talking={client.talking} />
          <span className={cn("min-w-0 truncate", silenced && "text-muted", isMe && "font-medium")} style={color ? { color } : undefined}>
            {client.nickname}
          </span>
          <TsGroupBadges groups={client.groups} channelGroup={client.channel_group} />
          <LiveBadge client={client} name={client.nickname} />
          {client.away !== null && (
            <span className="min-w-0 max-w-[40%] truncate text-xs text-subtle" title={client.away}>
              {client.away || t("tree.away")}
            </span>
          )}
          <span className="ml-auto flex shrink-0 items-center gap-1.5">
            {client.server_muted && !client.server_deafened && (
              <MicOff aria-label={t("tree.serverMuted")} className="size-3.5 text-warn">
                <title>{t("tree.serverMuted")}</title>
              </MicOff>
            )}
            {client.server_deafened && (
              <HeadphoneOff aria-label={t("tree.serverDeafened")} className="size-3.5 text-warn">
                <title>{t("tree.serverDeafened")}</title>
              </HeadphoneOff>
            )}
            {!client.server_muted && !client.server_deafened && client.muted && !client.deafened && <MicOff aria-label={t("tree.muted")} className="size-3.5 text-danger" />}
            {!client.server_deafened && client.deafened && <HeadphoneOff aria-label={t("tree.deafened")} className="size-3.5 text-danger" />}
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
          <ContextMenu.Item className={menuItem} onSelect={() => useUi.getState().openDialog({ kind: "memberProfile", uid: client.uid, fallback: client.nickname })}>
            {t("member.viewProfile")}
          </ContextMenu.Item>
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
            <ContextMenu.Item className={menuItem} onSelect={() => void controller.moveClient(client.id, client.uid, myChannel)}>
              <MoveRight className="size-4" /> {t("tree.moveHere")}
            </ContextMenu.Item>
          )}
          <ModerationItems person={{ uid: client.uid, nickname: client.nickname, session: client.id }} groups={client.groups} />
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
