import * as Dropdown from "@radix-ui/react-dropdown-menu";
import { ChevronDown, Copy, KeyRound, LogOut, Plus, Pencil, Settings2, Ticket, UserPlus } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { ChannelTree } from "./ChannelTree";
import { usePermission } from "./hooks";
import { menuContent, menuItem, menuItemDanger, menuSeparator } from "./kit";
import { TeamSpeakBadge } from "./badges";
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
  const manageServer = usePermission("server_manage");
  const manageGroups = usePermission("group_manage");
  const banClients = usePermission("client_ban");
  const canManage = manageServer || (kind === "vc" && (manageGroups || banClients));
  const canInvite = usePermission("invite_create") && kind === "vc";
  const canCreate = usePermission("channel_create");
  const canToken = usePermission("token_create") && kind === "vc"; // TeamSpeak has no vc tokens

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
          className="t group flex h-12 w-full shrink-0 cursor-pointer items-center gap-2 border-b border-line px-4 text-left hover:bg-hover data-[state=open]:bg-hover"
          aria-label={t("server.menu")}
        >
          <span className="min-w-0 flex-1">
            <span className="flex items-center gap-1.5">
              <span className="truncate text-[15px] font-semibold leading-tight">{server?.name}</span>
              {kind === "teamspeak" && <TeamSpeakBadge />}
            </span>
            <span
              className={cn("block truncate text-[11px] leading-tight", phase === "reconnecting" ? "text-warn" : "text-subtle")}
            >
              {phase === "reconnecting" ? t("server.reconnecting") : address}
            </span>
          </span>
          <ChevronDown className="size-4 shrink-0 text-muted transition-transform duration-150 group-data-[state=open]:rotate-180" />
        </button>
      </Dropdown.Trigger>
      <Dropdown.Portal>
        <Dropdown.Content align="start" sideOffset={4} className={cn(menuContent, "w-64")}>
          {kind === "vc" && uid && (
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
          <Dropdown.Separator className={menuSeparator} />
          <Dropdown.Item className={cn(menuItem, menuItemDanger)} onSelect={() => void controller.disconnect()}>
            <LogOut className="size-4" /> {t("server.disconnect")}
          </Dropdown.Item>
        </Dropdown.Content>
      </Dropdown.Portal>
    </Dropdown.Root>
  );
}

export function Sidebar({ className }: { className?: string }) {
  const phase = useSession((s) => s.phase);
  return (
    <aside className={cn("flex h-full w-[280px] shrink-0 flex-col border-r border-line bg-side", className)}>
      <ServerHeader />
      <div className={cn("min-h-0 flex-1 overflow-y-auto transition-opacity duration-150", phase === "reconnecting" && "opacity-50")}>
        <ChannelTree />
      </div>
      <VoicePanel />
    </aside>
  );
}

