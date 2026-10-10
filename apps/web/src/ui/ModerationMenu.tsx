import * as ContextMenu from "@radix-ui/react-context-menu";
import { Ban, Pencil, ChevronRight, ShieldCheck, UserMinus, UserX } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { canActOn, canAssignGroup, ADMIN_GROUP, MEMBER_GROUP, orderedGroups, toggleGroup } from "../lib/permissions";
import { controller } from "../state/controller";
import { useSession, useUi, type PersonRef } from "../state/stores";
import { usePermission } from "./hooks";
import { menuContent, menuItem, menuItemDanger, menuSeparator } from "./kit";

/**
 * Kick, ban, remove and role items for the context menu of a person (in the channel
 * tree or the member list). Each shows only if I may use it; role toggles that
 * would break a rule are disabled instead.
 */
export function ModerationItems({ person, groups }: { person: PersonRef; groups: number[] }) {
  const t = useT();
  const isMe = useSession((s) => s.me?.uid === person.uid);
  const kind = useSession((s) => s.kind);
  const permissions = useSession((s) => s.permissions);
  const defs = useSession((s) => s.groups);
  const canKick = usePermission("client_kick");
  const canBan = usePermission("client_ban");
  const canGroups = usePermission("group_manage");
  const canRemove = usePermission("member_remove");
  const canRename = usePermission("member_nickname");
  // TeamSpeak servers keep their own bans and roles: only kicking is offered there, as before.
  const vc = kind === "vc";
  const stronger = vc && !canActOn(permissions, groups, defs);
  const showRename = vc && (isMe || canRename);
  const showKick = !isMe && canKick && person.session !== undefined;
  const showBan = !isMe && vc && canBan;
  const showRoles = !isMe && vc && canGroups;
  const showRemove = !isMe && vc && canRemove;
  if (!showRename && !showKick && !showBan && !showRoles && !showRemove) return null;

  const roles = orderedGroups(defs).filter((g) => g.id !== MEMBER_GROUP);

  return (
    <>
      <ContextMenu.Separator className={menuSeparator} />
      {showRename && (
        <ContextMenu.Item className={menuItem} onSelect={() => useUi.getState().openDialog({ kind: "nickname", uid: person.uid })}>
          <Pencil className="size-4" /> {t("nickname.change")}
        </ContextMenu.Item>
      )}
      {showRoles && (
        <ContextMenu.Sub>
          <ContextMenu.SubTrigger className={cn(menuItem, "justify-between")}>
            <span className="flex items-center gap-2">
              <ShieldCheck className="size-4" /> {t("mod.roles")}
            </span>
            <ChevronRight className="size-4 text-subtle" />
          </ContextMenu.SubTrigger>
          <ContextMenu.Portal>
            <ContextMenu.SubContent className={cn(menuContent, "w-56")} sideOffset={4}>
              {roles.length === 0 && <div className="px-2 py-1.5 text-xs text-subtle">{t("mod.noRoles")}</div>}
              {roles.map((g) => {
                const on = groups.includes(g.id);
                // I may only hand out what I hold myself, and not act on someone stronger than me.
                const blocked = stronger || !canAssignGroup(permissions, g);
                return (
                  <ContextMenu.CheckboxItem
                    key={g.id}
                    checked={on}
                    disabled={blocked}
                    title={blocked ? t("mod.roleBlocked") : undefined}
                    onCheckedChange={(next) => {
                      void controller.attempt(controller.setMemberGroups(person.uid, toggleGroup(groups, g.id, next)));
                    }}
                    className={cn(menuItem, "pl-7")}
                  >
                    <ContextMenu.ItemIndicator className="absolute left-2 text-accent">
                      <span className="block size-2 rounded-full bg-accent" />
                    </ContextMenu.ItemIndicator>
                    <span className="size-2.5 shrink-0 rounded-full border border-line-strong" style={{ background: g.color ?? "transparent" }} />
                    <span className="min-w-0 flex-1 truncate">{g.name}</span>
                    {g.id === ADMIN_GROUP && <ShieldCheck className="size-3.5 text-subtle" />}
                  </ContextMenu.CheckboxItem>
                );
              })}
            </ContextMenu.SubContent>
          </ContextMenu.Portal>
        </ContextMenu.Sub>
      )}
      {showKick && (
        <ContextMenu.Item
          className={cn(menuItem, menuItemDanger)}
          disabled={stronger}
          onSelect={() =>
            useUi.getState().openDialog({
              kind: "confirm",
              title: t("tree.kickTitle", { name: person.nickname }),
              body: t("tree.kickBody"),
              confirmLabel: t("tree.kick"),
              danger: true,
              onConfirm: () => void controller.kickClient(person.session!),
            })
          }
        >
          <UserX className="size-4" /> {t("tree.kick")}
        </ContextMenu.Item>
      )}
      {showBan && (
        <ContextMenu.Item
          className={cn(menuItem, menuItemDanger)}
          disabled={stronger}
          onSelect={() => useUi.getState().openDialog({ kind: "ban", person })}
        >
          <Ban className="size-4" /> {t("mod.ban")}
        </ContextMenu.Item>
      )}
      {showRemove && (
        <ContextMenu.Item
          className={cn(menuItem, menuItemDanger)}
          disabled={stronger}
          onSelect={() => useUi.getState().openDialog({ kind: "removeMember", person })}
        >
          <UserMinus className="size-4" /> {t("mod.remove")}
        </ContextMenu.Item>
      )}
    </>
  );
}
