import { useMemo, useState } from "react";
import { useT } from "../i18n";
import { ADMIN_GROUP } from "../lib/permissions";
import type { ChannelAccess } from "../proto/ChannelAccess";
import { useSession } from "../state/stores";
import { useMyGroups } from "./hooks";
import { Input } from "./kit";

/** Who a private channel admits besides admins: roles and members (docs/e2e-build.md, section 6). */
export function AccessPicker({ value, onChange }: { value: ChannelAccess; onChange: (access: ChannelAccess) => void }) {
  const t = useT();
  const groups = useSession((s) => s.groups);
  const members = useSession((s) => s.members);
  const myGroups = useMyGroups();
  const [filter, setFilter] = useState("");
  // A role is listed only by someone holding it or Admin; one already listed can always be removed.
  const roles = useMemo(
    () =>
      Object.values(groups)
        .filter((g) => g.id !== ADMIN_GROUP && (myGroups.includes(ADMIN_GROUP) || myGroups.includes(g.id) || value.roles.includes(g.id)))
        .sort((a, b) => a.name.localeCompare(b.name)),
    [groups, myGroups, value.roles],
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
      <p className="text-xs text-subtle">{t("channel.accessHint")}</p>
      {roles.length > 0 && (
        <fieldset className="flex flex-col gap-1">
          <legend className="mb-1 text-xs font-medium text-subtle">{t("channel.accessRoles")}</legend>
          {roles.map((role) => (
            <label key={role.id} className="flex cursor-pointer items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={value.roles.includes(role.id)}
                onChange={() => onChange({ ...value, roles: toggle(value.roles, role.id) })}
                className="size-4 accent-[var(--accent)]"
              />
              {role.name}
            </label>
          ))}
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
