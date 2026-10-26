import * as ContextMenu from "@radix-ui/react-context-menu";
import { Plus } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useBookmarks, useSettings } from "../state/settings";
import { useUi } from "../state/stores";
import { TeamSpeakBadge } from "./badges";
import { Avatar, Tooltip, menuContent, menuItem, menuItemDanger } from "./kit";
import { useBookmarkOpener } from "./nav";

export function ServerRail() {
  const t = useT();
  const bookmarks = useBookmarks();
  const openDialog = useUi((s) => s.openDialog);
  const { isActive, open } = useBookmarkOpener();

  return (
    <nav aria-label={t("rail.label")} className="flex h-full w-16 shrink-0 flex-col items-center bg-rail py-3">
      <div className="flex min-h-0 flex-1 flex-col items-center gap-2 overflow-y-auto overflow-x-hidden px-2.5 py-1.5">
        {bookmarks.map((b) => {
          const active = isActive(b);
          return (
            <ContextMenu.Root key={b.id}>
              <ContextMenu.Trigger asChild>
                <div className="relative flex w-full justify-center">
                  <Tooltip label={`${b.name}${b.kind === "teamspeak" ? ` · ${t("badge.teamspeak")}` : ""}`} side="right">
                    <button
                      onClick={() => open(b)}
                      aria-label={b.name}
                      aria-current={active ? "true" : undefined}
                      className={cn(
                        "t cursor-pointer overflow-hidden transition-[border-radius,box-shadow] duration-150",
                        active ? "rounded-xl ring-2 ring-accent ring-offset-2 ring-offset-rail" : "rounded-[22px] hover:rounded-xl",
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
    </nav>
  );
}
