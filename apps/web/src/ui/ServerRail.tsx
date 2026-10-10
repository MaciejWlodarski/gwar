import * as ContextMenu from "@radix-ui/react-context-menu";
import { Plus, Settings } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller } from "../state/controller";
import { useSettings, type Bookmark } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { TeamSpeakBadge } from "./badges";
import { Avatar, IconButton, Tooltip, menuContent, menuItem, menuItemDanger } from "./kit";

export function ServerRail() {
  const t = useT();
  const bookmarks = useSettings((s) => s.bookmarks);
  const phase = useSession((s) => s.phase);
  const address = useSession((s) => s.address);
  const kind = useSession((s) => s.kind);
  const openDialog = useUi((s) => s.openDialog);
  const setDrawer = useUi((s) => s.setDrawer);

  const isActive = (b: Bookmark) =>
    phase !== "idle" &&
    (b.kind ?? "vc") === kind &&
    b.address.trim().toLowerCase() === address.trim().toLowerCase();

  const open = (b: Bookmark) => {
    if (isActive(b) && phase === "online") {
      setDrawer(false);
      return;
    }
    setDrawer(false);
    void controller.connectInteractive({ kind: b.kind, address: b.address, password: b.password, identity: b.identity }, { remember: false });
  };

  return (
    <nav aria-label={t("rail.label")} className="flex h-full w-[72px] shrink-0 flex-col items-center gap-2 bg-rail py-3">
      <div className="flex min-h-0 flex-1 flex-col items-center gap-2 overflow-y-auto overflow-x-hidden px-3 py-1">
        {bookmarks.map((b) => {
          const active = isActive(b);
          return (
            <ContextMenu.Root key={b.id}>
              <ContextMenu.Trigger asChild>
                <div className="relative flex w-full justify-center">
                  <span
                    className={cn(
                      "absolute top-1/2 -left-3 w-1 -translate-y-1/2 rounded-r-full bg-fg transition-all duration-150",
                      active ? "h-8 opacity-100" : "h-0 opacity-0",
                    )}
                  />
                  <Tooltip label={`${b.name}${b.kind === "teamspeak" ? ` · ${t("badge.teamspeak")}` : ""}`} side="right">
                    <button
                      onClick={() => open(b)}
                      aria-label={b.name}
                      aria-current={active ? "true" : undefined}
                      className={cn(
                        "t cursor-pointer overflow-hidden transition-[border-radius] duration-150",
                        active ? "rounded-xl" : "rounded-[22px] hover:rounded-xl",
                      )}
                    >
                      <Avatar name={b.name} seed={b.address} size={44} square className="!rounded-none" />
                    </button>
                  </Tooltip>
                  {b.kind === "teamspeak" && (
                    <TeamSpeakBadge short className="pointer-events-none absolute right-0 -bottom-0.5 ring-2 ring-rail" />
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
                  <ContextMenu.Item
                    className={cn(menuItem, menuItemDanger)}
                    onSelect={() => useSettings.getState().removeBookmark(b.id)}
                  >
                    {t("rail.remove")}
                  </ContextMenu.Item>
                </ContextMenu.Content>
              </ContextMenu.Portal>
            </ContextMenu.Root>
          );
        })}
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
      <IconButton label={t("settings.title")} side="right" onClick={() => openDialog({ kind: "settings", tab: "audio" })}>
        <Settings className="size-5" />
      </IconButton>
    </nav>
  );
}
