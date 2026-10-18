import { useMemo, useState } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { ADMIN_GROUP, canGiveGroup, orderedGroups } from "../lib/permissions";
import type { ChannelAccess } from "../proto/ChannelAccess";
import { useSession } from "../state/stores";
import { useMyWho } from "./hooks";
import { Input } from "./kit";

/** Who a private channel admits besides admins: roles and members (docs/e2e-build.md, section 6).
 * `membersOnly`: an encrypted private channel's signed member list (6c): roles are not stored but
 * expanded to their current members ("Add everyone with role…"). */
export function AccessPicker({ value, onChange, membersOnly = false, disabled = false }: { value: ChannelAccess; onChange: (access: ChannelAccess) => void; membersOnly?: boolean; disabled?: boolean }) {
  const t = useT();
  const groups = useSession((s) => s.groups);
  const members = useSession((s) => s.members);
  const me = useMyWho();
  const [filter, setFilter] = useState("");
  // A role is listed by someone holding it or ranked above it; one already listed can always be removed.
  const roles = useMemo(
    () =>
      orderedGroups(groups).filter(
        (g) => g.id !== ADMIN_GROUP && (me.groups.includes(g.id) || canGiveGroup(me, g, groups) || value.roles.includes(g.id)),
      ),
    [groups, me, value.roles],
  );
  const people = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return Object.values(members)
      // TeamSpeak users are never admitted to a private channel.
      .filter((m) => !m.uid.startsWith("ts:") && (!needle || m.nickname.toLowerCase().includes(needle)))
      .sort((a, b) => Number(value.users.includes(b.uid)) - Number(value.users.includes(a.uid)) || a.nickname.localeCompare(b.nickname))
      .slice(0, 200);
  }, [members, filter, value.users]);
  const toggle = <T,>(list: T[], item: T) => (list.includes(item) ? list.filter((x) => x !== item) : [...list, item]);

  return (
    <div className="flex flex-col gap-2 rounded-md border border-line p-3">
      <p className="text-sm font-medium">{t("channel.access")}</p>
      <p className="text-xs text-subtle">{t(membersOnly ? "channel.membersHint" : "channel.accessHint")}</p>
      {membersOnly && roles.length > 0 && (
        <div className="flex flex-wrap gap-1">
          {roles.map((role) => (
            <button
              key={role.id}
              type="button"
              disabled={disabled}
              className="rounded border border-line px-2 py-0.5 text-xs hover:bg-hover disabled:opacity-50"
              onClick={() => {
                const add = Object.values(members).filter((m) => !m.uid.startsWith("ts:") && m.groups.includes(role.id)).map((m) => m.uid);
                onChange({ ...value, users: [...new Set([...value.users, ...add])] });
              }}
            >
              {t("channel.addRole", { role: role.name })}
            </button>
          ))}
        </div>
      )}
      {!membersOnly && roles.length > 0 && (
        <fieldset className="flex flex-col gap-1">
          <legend className="mb-1 text-xs font-medium text-subtle">{t("channel.accessRoles")}</legend>
          {roles.map((role) => {
            // Only roles I hold or rank above can be added or taken away; others stay as they are.
            const locked = !me.groups.includes(role.id) && !canGiveGroup(me, role, groups);
            return (
              <label key={role.id} className={cn("flex items-center gap-2 text-sm", locked ? "cursor-not-allowed opacity-60" : "cursor-pointer")}>
                <input
                  type="checkbox"
                  checked={value.roles.includes(role.id)}
                  disabled={disabled || locked}
                  onChange={() => onChange({ ...value, roles: toggle(value.roles, role.id) })}
                  className="size-4 accent-[var(--accent)]"
                />
                {role.name}
              </label>
            );
          })}
        </fieldset>
      )}
      <fieldset className="flex flex-col gap-1">
        <legend className="mb-1 text-xs font-medium text-subtle">{t("channel.accessMembers")}</legend>
        <Input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder={t("channel.accessFilter")} aria-label={t("channel.accessFilter")} />
        <div className="flex max-h-40 flex-col gap-1 overflow-y-auto">
          {people.map((m) => (
            <label key={m.uid} className="flex cursor-pointer items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={value.users.includes(m.uid)}
                disabled={disabled}
                onChange={() => onChange({ ...value, users: toggle(value.users, m.uid) })}
                className="size-4 accent-[var(--accent)]"
              />
              <span className="truncate">{m.nickname}</span>
            </label>
          ))}
        </div>
      </fieldset>
    </div>
  );
}
