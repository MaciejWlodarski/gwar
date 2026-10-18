import { Check, Minus, X } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import { useT, type Key } from "../i18n";
import { cn } from "../lib/cn";
import { CHANNEL_PERMISSIONS, channelPermissions, effectiveOverrides, overrideSource } from "../lib/channel-permissions";
import { MEMBER_GROUP, canActOn, canGiveGroup, isHexColor, orderedGroups } from "../lib/permissions";
import type { ChannelOverride } from "../proto/ChannelOverride";
import type { Permission } from "../proto/Permission";
import { controller } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { useMyWho } from "./hooks";
import { Avatar, Button, Dialog, Select } from "./kit";

type Target = { role: number } | { user: string };
type State = "deny" | "inherit" | "allow";

const sameTarget = (o: ChannelOverride, t: Target) => ("role" in t ? o.role === t.role : o.user === t.user);
const targetKey = (t: Target) => ("role" in t ? `r${t.role}` : `u${t.user}`);

/**
 * A channel's overrides (docs/roles.md, "Overrides"): who is listed, and for each permission
 * deny, inherit or allow. Every change is sent at once; the server checks rank and permissions.
 */
export function ChannelPermissionsDialog({ channel }: { channel: number }) {
  const t = useT();
  const close = () => useUi.getState().closeDialog();
  const channels = useSession((s) => s.channels);
  const groups = useSession((s) => s.groups);
  const members = useSession((s) => s.members);
  const ownerUid = useSession((s) => s.owner?.uid);
  const meUid = useSession((s) => s.me?.uid ?? "");
  const me = useMyWho();
  const row = channels[channel];
  const parent = row?.parent != null ? channels[row.parent] : undefined;
  const source = overrideSource(channels, channel);
  const overrides = useMemo(() => effectiveOverrides(channels, channel), [channels, channel]);
  const mine = useMemo(() => channelPermissions({ uid: meUid, ...me }, channel, channels, groups), [meUid, me, channel, channels, groups]);
  const [selected, setSelected] = useState<Target>({ role: MEMBER_GROUP });
  const [busy, setBusy] = useState(false);

  const targets = useMemo(() => {
    const listed: Target[] = [{ role: MEMBER_GROUP }];
    for (const g of orderedGroups(groups)) {
      if (g.id !== MEMBER_GROUP && overrides.some((o) => o.role === g.id)) listed.push({ role: g.id });
    }
    for (const o of overrides) if (o.user) listed.push({ user: o.user });
    return listed;
  }, [groups, overrides]);

  // What I may touch: roles below my rank, people ranked below me.
  const editable = (target: Target) =>
    "role" in target
      ? !!groups[target.role] && canGiveGroup(me, groups[target.role]!, groups)
      : target.user !== meUid && canActOn(me, { groups: members[target.user]?.groups ?? [], owner: target.user === ownerUid }, groups);

  const addable = useMemo(() => {
    const roles = orderedGroups(groups)
      .filter((g) => g.id !== MEMBER_GROUP && !overrides.some((o) => o.role === g.id) && canGiveGroup(me, g, groups))
      .map((g) => ({ value: `r${g.id}`, label: g.name }));
    const people = Object.values(members)
      .filter((m) => m.uid !== meUid && !overrides.some((o) => o.user === m.uid))
      .filter((m) => canActOn(me, { groups: m.groups, owner: m.uid === ownerUid }, groups))
      .sort((a, b) => a.nickname.localeCompare(b.nickname))
      .map((m) => ({ value: `u${m.uid}`, label: `@${m.nickname}` }));
    return [...roles, ...people];
  }, [groups, members, overrides, me, meUid, ownerUid]);

  if (!row) return null;
  const entry = overrides.find((o) => sameTarget(o, selected)) ?? { allow: [], deny: [] };
  const stateOf = (p: Permission): State => (entry.allow.includes(p) ? "allow" : entry.deny.includes(p) ? "deny" : "inherit");
  const canEditSelected = editable(selected);

  const send = async (next: ChannelOverride) => {
    setBusy(true);
    await controller.attempt(controller.setChannelOverride(channel, next));
    setBusy(false);
  };
  const setState = (p: Permission, state: State) => {
    const allow = entry.allow.filter((x) => x !== p);
    const deny = entry.deny.filter((x) => x !== p);
    if (state === "allow") allow.push(p);
    if (state === "deny") deny.push(p);
    void send({ ...selected, allow, deny });
  };
  const sync = (on: boolean) => void controller.attempt(controller.updateChannel({ channel, synced: on }));

  // Who could join with just one role (and Member): the picture of the rule at a glance.
  const joiners = orderedGroups(groups).filter((g) =>
    channelPermissions({ uid: "", groups: g.id === MEMBER_GROUP ? [] : [g.id] }, channel, channels, groups).includes("connect"),
  );

  const label = (target: Target) => {
    if ("role" in target) return target.role === MEMBER_GROUP ? t("chperm.everyone") : (groups[target.role]?.name ?? "?");
    return `@${members[target.user]?.nickname ?? target.user.slice(0, 8)}`;
  };

  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("chperm.title", { name: row.name })} description={t("chperm.description")} width="max-w-3xl">
      <div className="flex flex-col gap-4">
        {parent && (
          <div className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-hover px-3 py-2 text-sm">
            <span className="text-muted">{row.synced ? t("chperm.synced", { parent: parent.name }) : t("chperm.own")}</span>
            <Button size="sm" onClick={() => sync(!row.synced)}>
              {row.synced ? t("chperm.unsync") : t("chperm.sync", { parent: parent.name })}
            </Button>
          </div>
        )}
        {row.synced && source && source.id !== channel && <p className="text-xs text-subtle">{t("chperm.editingSynced", { parent: source.name })}</p>}
        <div className="grid gap-4 md:grid-cols-[14rem_1fr]">
          <div className="flex flex-col gap-1">
            <ul aria-label={t("chperm.targets")} className="flex flex-col gap-0.5">
              {targets.map((target) => {
                const active = targetKey(target) === targetKey(selected);
                const role = "role" in target ? groups[target.role] : undefined;
                return (
                  <li key={targetKey(target)}>
                    <button
                      type="button"
                      onClick={() => setSelected(target)}
                      aria-current={active}
                      className={cn("t flex h-9 w-full cursor-pointer items-center gap-2 rounded-md px-2 text-left text-sm", active ? "bg-active text-fg" : "text-muted hover:bg-hover hover:text-fg")}
                    >
                      {"user" in target ? (
                        <Avatar name={members[target.user]?.nickname ?? target.user} seed={target.user} size={18} />
                      ) : (
                        <span className="size-3 shrink-0 rounded-full border border-line-strong" style={{ background: isHexColor(role?.color) ? role!.color! : "transparent" }} />
                      )}
                      <span className="min-w-0 flex-1 truncate">{label(target)}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
            {addable.length > 0 && (
              <Select
                value=""
                placeholder={t("chperm.add")}
                options={addable}
                onValueChange={(v) => setSelected(v.startsWith("r") ? { role: Number(v.slice(1)) } : { user: v.slice(1) })}
              />
            )}
          </div>
          <div className="flex min-w-0 flex-col">
            {!canEditSelected && <p className="mb-2 rounded-md bg-hover px-3 py-2 text-sm text-muted">{t("chperm.locked")}</p>}
            <ul className="flex flex-col divide-y divide-line">
              {CHANNEL_PERMISSIONS.map((p) => {
                const state = stateOf(p);
                // Changing a permission either way needs it here.
                const disabled = busy || !canEditSelected || !mine.includes(p);
                return (
                  <li key={p} className="flex items-center gap-3 py-2">
                    <span className="min-w-0 flex-1">
                      <span className="block text-sm text-fg">{t(`perm.${p}` as Key)}</span>
                      <span className="block text-xs text-subtle">{t(`perm.${p}.hint` as Key)}</span>
                    </span>
                    <TriState value={state} disabled={disabled} onChange={(next) => setState(p, next)} />
                  </li>
                );
              })}
            </ul>
            <p className="mt-3 text-xs text-subtle">
              {t("chperm.whoJoins")} {joiners.length ? joiners.map((g) => (g.id === MEMBER_GROUP ? t("chperm.everyone") : g.name)).join(", ") : "—"}
            </p>
          </div>
        </div>
      </div>
    </Dialog>
  );
}

function TriState({ value, disabled, onChange }: { value: State; disabled: boolean; onChange: (v: State) => void }) {
  const t = useT();
  const options: Array<{ v: State; icon: ReactNode; on: string }> = [
    { v: "deny", icon: <X className="size-3.5" />, on: "bg-danger-soft text-danger" },
    { v: "inherit", icon: <Minus className="size-3.5" />, on: "bg-active text-fg" },
    { v: "allow", icon: <Check className="size-3.5" />, on: "bg-ok/15 text-ok" },
  ];
  return (
    <div role="radiogroup" className="flex shrink-0 overflow-hidden rounded-md border border-line-strong">
      {options.map((o) => (
        <button
          key={o.v}
          type="button"
          role="radio"
          aria-checked={value === o.v}
          aria-label={t(`chperm.${o.v}` as Key)}
          title={t(`chperm.${o.v}` as Key)}
          disabled={disabled}
          onClick={() => value !== o.v && onChange(o.v)}
          className={cn("t flex h-7 w-8 items-center justify-center disabled:cursor-not-allowed", value === o.v ? o.on : "text-subtle hover:bg-hover", !disabled && "cursor-pointer")}
        >
          {o.icon}
        </button>
      ))}
    </div>
  );
}

