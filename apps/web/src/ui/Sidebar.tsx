import * as Dropdown from "@radix-ui/react-dropdown-menu";
import { ChevronDown, ChevronRight, Copy, Fingerprint, KeyRound, LayoutDashboard, LogOut, Plus, Pencil, Settings2, Ticket, UserPlus } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { ChannelTree } from "./ChannelTree";
import { usePermission } from "./hooks";
import { menuContent, menuItem, menuItemDanger, menuSeparator, panelCard } from "./kit";
import { TeamSpeakBadge, TsIcon } from "./badges";
import { VoicePanel } from "./VoicePanel";

function ServerHeader() {
  const t = useT();
  const server = useSession((s) => s.server);
  const uid = useSession((s) => s.me?.uid);
  const address = useSession((s) => s.address);
  const phase = useSession((s) => s.phase);
  const kind = useSession((s) => s.kind);
  const openDialog = useUi((s) => s.openDialog);
  const toast = useUi((s) => s.toast);
  const manageServer = usePermission("manage_server");
  const manageGroups = usePermission("manage_roles");
  const banClients = usePermission("ban_members");
  const editChannels = usePermission("manage_channels");
  const owned = useSession((s) => !!s.owner);
  // The Ownership tab is for anyone with channel_edit on a server with an owner (confirmers too).
  const canManage = manageServer || (kind === "gwar" && (manageGroups || banClients || (owned && editChannels)));
  const canInvite = usePermission("create_invites") && kind === "gwar";
  const canCreate = usePermission("manage_channels");
  const canToken = usePermission("create_invites") && kind === "gwar"; // TeamSpeak has no Gwar tokens
  const serverKey = useSession((s) => s.serverKey);

  const copyAddress = async () => {
    try {
      await navigator.clipboard.writeText(address);
      toast("success", t("toast.copied"));
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };

  return (
    <Dropdown.Root>
      <Dropdown.Trigger asChild>
        <button
          className="t group m-2 mb-1 flex h-12 shrink-0 cursor-pointer items-center gap-2 rounded-xl px-3 text-left hover:bg-hover data-[state=open]:bg-hover"
          aria-label={t("server.menu")}
        >
          <span className="min-w-0 flex-1">
            <span className="flex items-center gap-1.5">
              {kind === "teamspeak" && <TsIcon token={server?.icon} />}
              <span className="truncate text-[15px] font-semibold leading-tight">{server?.name}</span>
              {kind === "teamspeak" && <TeamSpeakBadge />}
            </span>
            <span
              className={cn("block truncate text-[11px] leading-tight", phase === "reconnecting" ? "text-warn" : "text-subtle")}
            >
              {phase === "reconnecting" ? t("server.reconnecting") : t("server.connected")}
            </span>
          </span>
          <ChevronDown className="size-4 shrink-0 text-muted transition-transform duration-150 group-data-[state=open]:rotate-180" />
        </button>
      </Dropdown.Trigger>
      <Dropdown.Portal>
        <Dropdown.Content align="start" sideOffset={4} className={cn(menuContent, "w-64")}>
          {kind === "gwar" && uid && (
            <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "nickname", uid })}>
              <Pencil className="size-4" /> {t("nickname.change")}
            </Dropdown.Item>
          )}
          {canManage && (
            <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "serverSettings" })}>
              <Settings2 className="size-4" /> {t("server.settings")}
            </Dropdown.Item>
          )}
          {canInvite && (
            <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "invites" })}>
              <UserPlus className="size-4" /> {t("server.invite")}
            </Dropdown.Item>
          )}
          {canCreate && (
            <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "channelEdit", mode: "create", parent: null })}>
              <Plus className="size-4" /> {t("server.createChannel")}
            </Dropdown.Item>
          )}
          {(canManage || canCreate || canInvite) && <Dropdown.Separator className={menuSeparator} />}
          <Dropdown.Sub>
            <Dropdown.SubTrigger className={menuItem}><Settings2 className="size-4" /> {t("common.advanced")}<ChevronRight className="ml-auto size-4" /></Dropdown.SubTrigger>
            <Dropdown.Portal><Dropdown.SubContent sideOffset={4} className={cn(menuContent, "w-64")}>
                {canToken && (
                  <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "createToken" })}>
                    <Ticket className="size-4" /> {t("server.createToken")}
                  </Dropdown.Item>
                )}
                <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "redeem" })}>
                  <KeyRound className="size-4" /> {t("server.redeem")}
                </Dropdown.Item>
                <Dropdown.Separator className={menuSeparator} />
                <Dropdown.Item className={menuItem} onSelect={() => void copyAddress()}>
                  <Copy className="size-4" /> {t("server.copyAddress")}
                </Dropdown.Item>
                {serverKey && (
                  <Dropdown.Item className={menuItem} onSelect={() => openDialog({ kind: "serverKey" })}>
                    <Fingerprint className="size-4" /> {t("server.key")}
                  </Dropdown.Item>
                )}
            </Dropdown.SubContent></Dropdown.Portal>
          </Dropdown.Sub>
          <Dropdown.Separator className={menuSeparator} />
          <Dropdown.Item className={cn(menuItem, menuItemDanger)} onSelect={() => void controller.disconnect()}>
            <LogOut className="size-4" /> {t("server.disconnect")}
          </Dropdown.Item>
        </Dropdown.Content>
      </Dropdown.Portal>
    </Dropdown.Root>
  );
}

/** The server's overview, above its channels: who talks where, what is new. */
function OverviewRow() {
  const t = useT();
  const selected = useUi((s) => s.overview);
  const showOverview = useUi((s) => s.showOverview);
  return (
    <div className="px-2 pt-2">
      <button
        onClick={showOverview}
        aria-current={selected ? "page" : undefined}
        className={cn(
          "t flex h-8 w-full cursor-pointer items-center gap-2 rounded-md px-2 text-left text-sm",
          selected ? "bg-active font-medium text-fg" : "text-muted hover:bg-hover hover:text-fg",
        )}
      >
        <LayoutDashboard className="size-4 shrink-0 text-subtle" />
        <span className="truncate">{t("overview.title")}</span>
      </button>
    </div>
  );
}

/** The server's channels and my voice panel: a column of the server view, or (`fill`) a phone's page. */
export function Sidebar({ card, fill }: { card?: boolean; fill?: boolean }) {
  const phase = useSession((s) => s.phase);
  return (
    <aside className={cn("flex h-full flex-col bg-side", fill ? "min-w-0 flex-1" : "w-[280px] shrink-0", card && panelCard)}>
      <ServerHeader />
      <div className={cn("min-h-0 flex-1 overflow-y-auto transition-opacity duration-150", phase === "reconnecting" && "opacity-50")}>
        <OverviewRow />
        <ChannelTree />
      </div>
      <VoicePanel />
    </aside>
  );
}

