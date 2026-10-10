import * as Dropdown from "@radix-ui/react-dropdown-menu";
import * as Tabs from "@radix-ui/react-tabs";
import { Ban, Check, Copy, Link2, Lock, Plus, Search, ShieldCheck, Trash2, UserMinus, UserPlus, Users, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { countKey, useLanguage, useT, type Key } from "../i18n";
import { INVITE_EXPIRY, INVITE_USES } from "../lib/ban";
import { cn } from "../lib/cn";
import {
  ADMIN_GROUP,
  ALL_PERMISSIONS,
  MEMBER_GROUP,
  PERMISSION_SECTIONS,
  canActOn,
  canAssignGroup,
  isBuiltinGroup,
  isHexColor,
  nameColor,
  orderedGroups,
  toggleGroup,
} from "../lib/permissions";
import { PRUNE_DAYS_DEFAULT, PRUNE_DAYS_MAX, parsePruneDays, pruneAll, pruneKey, pruneRequest, type PruneForm, type PruneReply } from "../lib/prune";
import { formatRelative, formatUntil } from "../lib/time";
import { webOrigin } from "../lib/official";
import { buildInviteLink } from "../net/invite";
import type { Ban as BanEntry } from "../proto/Ban";
import type { Group } from "../proto/Group";
import type { Invite } from "../proto/Invite";
import type { Permission } from "../proto/Permission";
import { controller, describeRequestError } from "../state/controller";
import { sortedChannels } from "../state/reducer";
import { useSession, useUi, type ServerSettingsTab } from "../state/stores";
import { usePermission } from "./hooks";
import { Avatar, Button, Dialog, EmptyState, Field, IconButton, Input, Select, Spinner, Switch, Textarea, menuContent, menuItem } from "./kit";

// ------------------------------------------------------------------ helpers

function ErrorLine({ children }: { children: ReactNode }) {
  if (!children) return null;
  return (
    <p role="alert" className="rounded-md bg-danger-soft px-3 py-2 text-sm text-danger">
      {children}
    </p>
  );
}

function RoleChip({ group, onRemove }: { group: Group; onRemove?: () => void }) {
  const color = isHexColor(group.color) ? group.color : undefined;
  return (
    <span className="inline-flex h-5 max-w-40 items-center gap-1 rounded-full border border-line-strong bg-side px-2 text-[11px] text-fg">
      <span className="size-2 shrink-0 rounded-full" style={{ background: color ?? "var(--subtle)" }} />
      <span className="truncate">{group.name}</span>
      {onRemove && (
        <button type="button" onClick={onRemove} aria-label={group.name} className="cursor-pointer text-subtle hover:text-fg">
          <X className="size-3" />
        </button>
      )}
    </span>
  );
}

function useCopy() {
  const t = useT();
  const toast = useUi((s) => s.toast);
  return useCallback(
    async (text: string) => {
      try {
        await navigator.clipboard.writeText(text);
        toast("success", t("toast.copied"));
        return true;
      } catch {
        toast("error", t("toast.copyFailed"));
        return false;
      }
    },
    [t, toast],
  );
}

// ------------------------------------------------------------------- dialog

export function ServerSettingsDialog({ tab }: { tab?: ServerSettingsTab }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const kind = useSession((s) => s.kind);
  const canServer = usePermission("server_manage");
  const canGroups = usePermission("group_manage");
  const canBan = usePermission("client_ban");
  const canInvite = usePermission("invite_create");
  const canRemove = usePermission("member_remove");
  const vc = kind === "vc";

  const tabs = useMemo(() => {
    const list: Array<{ id: ServerSettingsTab; label: string; icon: ReactNode }> = [];
    if (canServer) list.push({ id: "overview", label: t("ss.overview"), icon: <Lock className="size-4" /> });
    if (vc && (canGroups || canServer)) list.push({ id: "roles", label: t("ss.roles"), icon: <ShieldCheck className="size-4" /> });
    if (vc && (canGroups || canServer || canBan || canRemove)) list.push({ id: "members", label: t("ss.members"), icon: <Users className="size-4" /> });
    if (vc && canRemove) list.push({ id: "cleanup", label: t("ss.cleanup"), icon: <UserMinus className="size-4" /> });
    if (vc && canBan) list.push({ id: "bans", label: t("ss.bans"), icon: <Ban className="size-4" /> });
    if (vc && canInvite) list.push({ id: "invites", label: t("ss.invites"), icon: <Link2 className="size-4" /> });
    return list;
  }, [t, vc, canServer, canGroups, canBan, canRemove, canInvite]);

  const [chosen, setCurrent] = useState<ServerSettingsTab>(tab ?? "overview");
  // Permissions can change under us (a role edit): fall back from tabs that are no longer allowed.
  const current = tabs.some((x) => x.id === chosen) ? chosen : (tabs[0]?.id ?? "overview");
  useEffect(() => {
    if (tabs.length === 0) close();
  }, [tabs, close]);

  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("server.settings")} width="max-w-3xl">
      <Tabs.Root value={current} onValueChange={(v) => setCurrent(v as ServerSettingsTab)}>
        <Tabs.List className="-mx-1 mb-4 flex gap-1 overflow-x-auto border-b border-line px-1 pb-2" aria-label={t("server.settings")}>
          {tabs.map((x) => (
            <Tabs.Trigger
              key={x.id}
              value={x.id}
              className="t flex h-8 shrink-0 cursor-pointer items-center gap-2 rounded-md px-3 text-sm text-muted hover:bg-hover hover:text-fg data-[state=active]:bg-active data-[state=active]:text-fg"
            >
              {x.icon}
              {x.label}
            </Tabs.Trigger>
          ))}
        </Tabs.List>
        <Tabs.Content value="overview" className="outline-none">
          <OverviewTab />
        </Tabs.Content>
        <Tabs.Content value="roles" className="outline-none">
          <RolesTab />
        </Tabs.Content>
        <Tabs.Content value="members" className="outline-none">
          <MembersTab />
        </Tabs.Content>
        <Tabs.Content value="cleanup" className="outline-none">
          <CleanupTab />
        </Tabs.Content>
        <Tabs.Content value="bans" className="outline-none">
          <BansTab />
        </Tabs.Content>
        <Tabs.Content value="invites" className="outline-none">
          <InvitesPanel />
        </Tabs.Content>
      </Tabs.Root>
    </Dialog>
  );
}

/** "Invite people": the invites tab on its own, for members who may create invites but not manage the server. */
export function InvitesDialog() {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("invite.title")} description={t("invite.description")} width="max-w-xl">
      <InvitesPanel />
    </Dialog>
  );
}

// ----------------------------------------------------------------- overview

const NO_CHANNEL = "__keep";

function OverviewTab() {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const server = useSession((s) => s.server);
  const kind = useSession((s) => s.kind);
  const channels = useSession((s) => s.channels);
  const vc = kind === "vc";
  const [name, setName] = useState(server?.name ?? "");
  const [welcome, setWelcome] = useState(server?.welcome ?? "");
  const [defaultChannel, setDefaultChannel] = useState(String(server?.default_channel ?? NO_CHANNEL));
  const [maxClients, setMaxClients] = useState(String(server?.max_clients ?? ""));
  const [password, setPassword] = useState("");
  const [removePassword, setRemovePassword] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const options = useMemo(() => sortedChannels(Object.values(channels)).map((c) => ({ value: String(c.id), label: c.name })), [channels]);

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy || !name.trim()) return;
    const update: Parameters<typeof controller.updateServer>[0] = {};
    if (name.trim() !== server?.name) update.name = name.trim();
    if (welcome !== server?.welcome) update.welcome = welcome;
    if (vc) {
      if (defaultChannel !== NO_CHANNEL && Number(defaultChannel) !== server?.default_channel) update.default_channel = Number(defaultChannel);
      const max = Number(maxClients);
      if (!Number.isInteger(max) || max < 1 || max > 100_000) {
        setError(t("ss.maxInvalid"));
        return;
      }
      if (max !== server?.max_clients) update.max_clients = max;
      if (removePassword) update.password = "";
      else if (password) update.password = password;
    }
    setBusy(true);
    setError(null);
    try {
      if (Object.keys(update).length > 0) await controller.updateServer(update);
      useUi.getState().toast("success", t("common.saved"));
      setPassword("");
      setRemovePassword(false);
      setBusy(false);
    } catch (err) {
      setError(describeRequestError(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
      <Field label={t("serverSettings.name")}>
        {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={64} autoFocus />}
      </Field>
      <Field label={t("serverSettings.welcome")} hint={t("serverSettings.welcomeHint")}>
        {(id) => <Textarea id={id} rows={4} value={welcome} onChange={(e) => setWelcome(e.target.value)} maxLength={1000} />}
      </Field>
      {vc && (
        <>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label={t("ss.defaultChannel")} hint={t("ss.defaultChannelHint")}>
              {(id) => <Select id={id} value={defaultChannel} onValueChange={setDefaultChannel} options={options} />}
            </Field>
            <Field label={t("ss.maxClients")} hint={t("ss.maxClientsHint")}>
              {(id) => <Input id={id} inputMode="numeric" value={maxClients} onChange={(e) => setMaxClients(e.target.value.replace(/\D/g, ""))} maxLength={6} />}
            </Field>
          </div>
          <Field label={t("ss.password")} hint={removePassword ? t("channel.passwordWillRemove") : t("ss.passwordHint")}>
            {(id) => (
              <Input
                id={id}
                type="password"
                value={password}
                onChange={(e) => {
                  setPassword(e.target.value);
                  setRemovePassword(false);
                }}
                autoComplete="new-password"
                maxLength={128}
                placeholder={t("ss.passwordPlaceholder")}
              />
            )}
          </Field>
          <label className="flex cursor-pointer items-center gap-2 text-sm text-muted">
            <input
              type="checkbox"
              checked={removePassword}
              onChange={(e) => {
                setRemovePassword(e.target.checked);
                if (e.target.checked) setPassword("");
              }}
              className="size-4 accent-[var(--accent)]"
            />
            {t("ss.removePassword")}
          </label>
        </>
      )}
      <ErrorLine>{error}</ErrorLine>
      <div className="flex justify-end gap-2">
        <Button type="button" onClick={close}>
          {t("common.close")}
        </Button>
        <Button type="submit" variant="primary" busy={busy} disabled={!name.trim()}>
          {t("common.save")}
        </Button>
      </div>
    </form>
  );
}

// -------------------------------------------------------------------- roles

const COLOR_PRESETS = ["#e74c3c", "#e67e22", "#f1c40f", "#2ecc71", "#1abc9c", "#3498db", "#9b59b6", "#e91e63"];

function RolesTab() {
  const t = useT();
  const groups = useSession((s) => s.groups);
  const canManage = usePermission("group_manage");
  const list = useMemo(() => orderedGroups(groups), [groups]);
  const [chosen, setSelected] = useState<number | "new">(list.find((g) => g.id !== ADMIN_GROUP && g.id !== MEMBER_GROUP)?.id ?? list[0]?.id ?? "new");
  // A deleted role leaves the editor empty: fall back to the first one.
  const selected = chosen === "new" || groups[chosen] ? chosen : (list[0]?.id ?? "new");
  const current = selected === "new" ? undefined : groups[selected];

  return (
    <div className="grid gap-4 md:grid-cols-[13rem_1fr]">
      <div className="flex flex-col gap-1">
        <ul aria-label={t("ss.roles")} className="flex flex-col gap-0.5">
          {list.map((g) => (
            <li key={g.id}>
              <button
                type="button"
                onClick={() => setSelected(g.id)}
                aria-current={selected === g.id}
                className={cn(
                  "t flex h-9 w-full cursor-pointer items-center gap-2 rounded-md px-2 text-left text-sm",
                  selected === g.id ? "bg-active text-fg" : "text-muted hover:bg-hover hover:text-fg",
                )}
              >
                <span className="size-3 shrink-0 rounded-full border border-line-strong" style={{ background: isHexColor(g.color) ? g.color : "transparent" }} />
                <span className="min-w-0 flex-1 truncate">{g.name}</span>
                {isBuiltinGroup(g.id) && <Lock aria-label={t("ss.builtin")} className="size-3 shrink-0 text-subtle" />}
              </button>
            </li>
          ))}
        </ul>
        {canManage && (
          <Button size="sm" onClick={() => setSelected("new")} className={cn("mt-1 justify-start", selected === "new" && "border-accent text-accent")}>
            <Plus className="size-4" /> {t("ss.createRole")}
          </Button>
        )}
      </div>
      <RoleEditor key={selected} group={current} onSaved={(id) => setSelected(id)} />
    </div>
  );
}

function PermissionRow({
  permission,
  checked,
  disabled,
  onChange,
}: {
  permission: Permission;
  checked: boolean;
  disabled: boolean;
  onChange: (on: boolean) => void;
}) {
  const t = useT();
  return (
    <label className={cn("flex items-start gap-2.5 rounded-md px-1 py-1", disabled ? "cursor-not-allowed opacity-60" : "cursor-pointer hover:bg-hover")}>
      <input
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
        className="mt-0.5 size-4 shrink-0 accent-[var(--accent)]"
      />
      <span className="min-w-0">
        <span className="block text-sm text-fg">{t(`perm.${permission}` as Key)}</span>
        <span className="block text-xs text-subtle">{t(`perm.${permission}.hint` as Key)}</span>
      </span>
    </label>
  );
}

function RoleEditor({ group, onSaved }: { group: Group | undefined; onSaved: (id: number) => void }) {
  const t = useT();
  const mine = useSession((s) => s.permissions);
  const canManage = usePermission("group_manage");
  const creating = !group;
  const isAdmin = group?.id === ADMIN_GROUP;
  // A role with permissions I lack is out of my hands (the server enforces it too).
  const outranked = !!group && !canAssignGroup(mine, group);
  const readOnly = !canManage || outranked;
  const [name, setName] = useState(group?.name ?? "");
  const [color, setColor] = useState(group?.color ?? "");
  const [perms, setPerms] = useState<Permission[]>(isAdmin ? ALL_PERMISSIONS : (group?.permissions ?? []));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const colorOk = color === "" || isHexColor(color);
  const permsChanged = !!group && (perms.length !== group.permissions.length || perms.some((p) => !group.permissions.includes(p)));
  const dirty = creating || name.trim() !== group.name || color !== (group.color ?? "") || (!isAdmin && permsChanged);

  const save = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy || readOnly || !name.trim() || !colorOk) return;
    setBusy(true);
    setError(null);
    try {
      if (creating) {
        const r = await controller.createGroup({ name: name.trim(), permissions: perms, color: color || null });
        onSaved(r.group.id);
      } else {
        await controller.updateGroup({
          group: group.id,
          ...(name.trim() !== group.name ? { name: name.trim() } : {}),
          ...(!isAdmin && permsChanged ? { permissions: perms } : {}),
          ...(color !== (group.color ?? "") ? { color } : {}),
        });
        useUi.getState().toast("success", t("common.saved"));
      }
    } catch (err) {
      setError(describeRequestError(err));
    }
    setBusy(false);
  };

  const remove = () => {
    if (!group) return;
    useUi.getState().openDialog({
      kind: "confirm",
      title: t("ss.deleteRoleTitle", { name: group.name }),
      body: t("ss.deleteRoleBody"),
      confirmLabel: t("common.delete"),
      danger: true,
      // The confirm dialog replaces this one; come back to the roles afterwards.
      onConfirm: () =>
        void controller.attempt(controller.deleteGroup(group.id)).finally(() => useUi.getState().openDialog({ kind: "serverSettings", tab: "roles" })),
    });
  };

  return (
    <form onSubmit={(e) => void save(e)} className="flex min-w-0 flex-col gap-4">
      {outranked && <p className="rounded-md bg-hover px-3 py-2 text-sm text-muted">{t("ss.roleOutranked")}</p>}
      {!canManage && <p className="rounded-md bg-hover px-3 py-2 text-sm text-muted">{t("ss.readOnly")}</p>}
      <Field label={t("ss.roleName")}>
        {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={32} disabled={readOnly} />}
      </Field>
      <Field label={t("ss.roleColor")} error={colorOk ? undefined : t("ss.colorInvalid")}>
        {(id) => (
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              disabled={readOnly}
              onClick={() => setColor("")}
              aria-label={t("ss.noColor")}
              title={t("ss.noColor")}
              className={cn(
                "flex size-6 cursor-pointer items-center justify-center rounded-full border border-line-strong text-subtle disabled:cursor-not-allowed",
                color === "" && "ring-2 ring-accent",
              )}
            >
              <X className="size-3" />
            </button>
            {COLOR_PRESETS.map((c) => (
              <button
                key={c}
                type="button"
                disabled={readOnly}
                aria-label={c}
                onClick={() => setColor(c)}
                className={cn("size-6 cursor-pointer rounded-full disabled:cursor-not-allowed", color.toLowerCase() === c && "ring-2 ring-accent ring-offset-2 ring-offset-surface")}
                style={{ background: c }}
              />
            ))}
            <input
              type="color"
              aria-label={t("ss.pickColor")}
              disabled={readOnly}
              value={isHexColor(color) ? color : "#888888"}
              onChange={(e) => setColor(e.target.value)}
              className="size-6 cursor-pointer rounded border-0 bg-transparent p-0"
            />
            <Input id={id} value={color} onChange={(e) => setColor(e.target.value.trim())} placeholder="#rrggbb" maxLength={7} disabled={readOnly} className="w-28 font-mono" spellCheck={false} />
          </div>
        )}
      </Field>
      <fieldset className="flex flex-col gap-3" disabled={readOnly}>
        <legend className="mb-1 text-xs font-medium text-muted">{t("ss.permissions")}</legend>
        {isAdmin && <p className="text-xs text-subtle">{t("ss.adminAll")}</p>}
        {PERMISSION_SECTIONS.map((section) => (
          <div key={section.id}>
            <h4 className="mb-1 text-[11px] font-semibold tracking-wide text-subtle uppercase">{t(`perm.section.${section.id}` as Key)}</h4>
            {section.permissions.map((p) => (
              <PermissionRow
                key={p}
                permission={p}
                checked={perms.includes(p)}
                // Only what I hold myself can be handed out or taken away.
                disabled={readOnly || isAdmin || !mine.includes(p)}
                onChange={(on) => setPerms((cur) => (on ? [...cur, p] : cur.filter((x) => x !== p)))}
              />
            ))}
          </div>
        ))}
      </fieldset>
      <ErrorLine>{error}</ErrorLine>
      {canManage && (
        <div className="flex items-center justify-between gap-2">
          {group && !isBuiltinGroup(group.id) ? (
            <Button type="button" variant="ghost" onClick={remove} disabled={outranked} className="text-danger hover:text-danger">
              <Trash2 className="size-4" /> {t("common.delete")}
            </Button>
          ) : (
            <span />
          )}
          <Button type="submit" variant="primary" busy={busy} disabled={readOnly || !dirty || !name.trim() || !colorOk}>
            {creating ? t("common.create") : t("common.save")}
          </Button>
        </div>
      )}
    </form>
  );
}

// ------------------------------------------------------------------ members

function MembersTab() {
  const t = useT();
  const lang = useLanguage();
  const members = useSession((s) => s.members);
  const clients = useSession((s) => s.clients);
  const defs = useSession((s) => s.groups);
  const mine = useSession((s) => s.permissions);
  const meUid = useSession((s) => s.me?.uid);
  const canGroups = usePermission("group_manage");
  const canBan = usePermission("client_ban");
  const canRemove = usePermission("member_remove");
  const [query, setQuery] = useState("");
  const [now] = useState(() => Date.now());
  const online = useMemo(() => new Map(Object.values(clients).map((c) => [c.uid, c.id])), [clients]);
  const roles = useMemo(() => orderedGroups(defs).filter((g) => g.id !== MEMBER_GROUP), [defs]);
  const rows = useMemo(() => {
    const q = query.trim().toLowerCase();
    return Object.values(members)
      .filter((m) => !q || m.nickname.toLowerCase().includes(q))
      .sort(
        (a, b) =>
          Number(online.has(b.uid)) - Number(online.has(a.uid)) || a.nickname.localeCompare(b.nickname, undefined, { sensitivity: "base" }),
      );
  }, [members, query, online]);

  return (
    <div className="flex flex-col gap-3">
      {!canGroups && <p className="rounded-md bg-hover px-3 py-2 text-sm text-muted">{t("ss.readOnly")}</p>}
      <div className="relative">
        <Search className="pointer-events-none absolute top-2.5 left-3 size-4 text-subtle" />
        <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder={t("ss.searchMembers")} aria-label={t("ss.searchMembers")} className="pl-9" />
      </div>
      {rows.length === 0 ? (
        <EmptyState icon={<Users className="size-5" />} title={t("ss.noMembers")} />
      ) : (
        <ul className="flex flex-col divide-y divide-line rounded-lg border border-line">
          {rows.map((m) => {
            const color = nameColor(m.groups, defs);
            const isMe = m.uid === meUid;
            const session = online.get(m.uid);
            const stronger = !canActOn(mine, m.groups, defs);
            const assigned = m.groups.map((g) => defs[g]).filter((g): g is Group => !!g && g.id !== MEMBER_GROUP);
            return (
              <li key={m.uid} data-member={m.nickname} className="flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2">
                <span className={cn(session === undefined && "opacity-50")}>
                  <Avatar name={m.nickname} seed={m.uid} size={28} />
                </span>
                <div className="min-w-0 flex-1 basis-40">
                  <div className="truncate text-sm font-medium" style={color ? { color } : undefined}>
                    {m.nickname}
                    {isMe && <span className="ml-1.5 text-xs font-normal text-subtle">({t("ss.you")})</span>}
                  </div>
                  <div className="text-[11px] text-subtle">
                    {session !== undefined ? t("members.online") : m.last_seen > 0 ? t("members.lastSeen", { when: formatRelative(m.last_seen, now, lang) }) : t("members.lastSeenUnknown")}
                  </div>
                </div>
                <div className="flex min-w-0 flex-wrap items-center gap-1">
                  {assigned.map((g) => (
                    <RoleChip key={g.id} group={g} />
                  ))}
                  {canGroups && (
                    <RoleMenu uid={m.uid} groups={m.groups} roles={roles} stronger={stronger} isMe={isMe} nickname={m.nickname} />
                  )}
                </div>
                {canBan && !isMe && (
                  <IconButton
                    label={t("mod.banNamed", { name: m.nickname })}
                    tone="danger"
                    size="sm"
                    disabled={stronger}
                    onClick={() => useUi.getState().openDialog({ kind: "ban", person: { uid: m.uid, nickname: m.nickname, session }, back: "members" })}
                  >
                    <Ban className="size-4" />
                  </IconButton>
                )}
                {canRemove && !isMe && (
                  <IconButton
                    label={t("mod.removeNamed", { name: m.nickname })}
                    tone="danger"
                    size="sm"
                    disabled={stronger}
                    onClick={() => useUi.getState().openDialog({ kind: "removeMember", person: { uid: m.uid, nickname: m.nickname, session }, back: "members" })}
                  >
                    <UserMinus className="size-4" />
                  </IconButton>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

// ----------------------------------------------------------------- clean up

/** Removes people who have not been here for a while: preview first, then the real thing. */
function CleanupTab() {
  const t = useT();
  const lang = useLanguage();
  const [form, setForm] = useState<PruneForm>({ days: PRUNE_DAYS_DEFAULT, keepRoles: true, deleteMessages: false });
  const [preview, setPreview] = useState<{ key: string; reply: PruneReply } | null>(null);
  const [busy, setBusy] = useState<"preview" | "remove" | null>(null);
  const [progress, setProgress] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());

  const change = (next: Partial<PruneForm>) => {
    setForm((f) => ({ ...f, ...next }));
    setError(null);
  };
  const key = pruneKey(form);
  const daysValid = parsePruneDays(form.days) !== null;
  // A preview only vouches for the settings it ran with.
  const current = preview && preview.key === key ? preview.reply : null;

  const runPreview = async () => {
    const request = pruneRequest(form, true);
    if (!request || !key || busy) return;
    setBusy("preview");
    setError(null);
    try {
      const reply = await controller.pruneMembers(request);
      setNow(Date.now());
      setPreview({ key, reply });
    } catch (e) {
      setError(describeRequestError(e));
    } finally {
      setBusy(null);
    }
  };

  const runRemove = async () => {
    const request = pruneRequest(form, false);
    if (!request || !current || busy) return;
    setBusy("remove");
    setProgress(0);
    setError(null);
    try {
      const removed = await pruneAll(() => controller.pruneMembers(request), setProgress);
      useUi.getState().toast("success", t(countKey(lang, "ss.pruneDone", removed), { count: removed }));
      setPreview(null);
    } catch (e) {
      setError(describeRequestError(e));
      setPreview(null);
    } finally {
      setBusy(null);
    }
  };

  const shown = current?.members ?? [];
  return (
    <div className="flex flex-col gap-4">
      <p className="text-sm text-muted">{t("ss.cleanupIntro")}</p>
      <Field label={t("ss.pruneDays")} hint={t("ss.pruneDaysHint", { max: PRUNE_DAYS_MAX })} error={daysValid ? undefined : t("ss.pruneDaysInvalid", { max: PRUNE_DAYS_MAX })}>
        {(id) => (
          <Input
            id={id}
            inputMode="numeric"
            value={form.days}
            onChange={(e) => change({ days: e.target.value })}
            disabled={busy !== null}
            className="w-32"
          />
        )}
      </Field>
      <Switch checked={form.keepRoles} onCheckedChange={(v) => change({ keepRoles: v })} label={t("ss.pruneKeepRoles")} description={t("ss.pruneKeepRolesHint")} />
      <Switch checked={form.deleteMessages} onCheckedChange={(v) => change({ deleteMessages: v })} label={t("ss.pruneMessages")} description={t("ss.pruneMessagesHint")} />
      <p className="text-xs text-subtle">{t("ss.pruneAlwaysKept")}</p>
      <ErrorLine>{error}</ErrorLine>
      <div className="flex flex-wrap gap-2">
        <Button onClick={() => void runPreview()} busy={busy === "preview"} disabled={!daysValid || busy === "remove"}>
          {t("ss.prunePreview")}
        </Button>
        <Button
          variant="danger"
          onClick={() => void runRemove()}
          busy={busy === "remove"}
          disabled={!current || current.count === 0 || busy === "preview"}
        >
          {busy === "remove"
            ? t("ss.pruneRunning", { done: progress, total: current?.count ?? progress })
            : current && current.count > 0
              ? t(countKey(lang, "ss.pruneRemove", current.count), { count: current.count })
              : t("ss.pruneRemoveIdle")}
        </Button>
      </div>
      {current && (
        <section aria-label={t("ss.pruneListLabel")} className="flex flex-col gap-2">
          <p role="status" className="text-sm font-medium">
            {current.count === 0 ? t("ss.pruneNone") : t(countKey(lang, "ss.pruneFound", current.count), { count: current.count })}
          </p>
          {shown.length > 0 && (
            <ul className="flex max-h-72 flex-col divide-y divide-line overflow-y-auto rounded-lg border border-line">
              {shown.map((m) => (
                <li key={m.uid} data-prune={m.nickname} className="flex items-center gap-3 px-3 py-1.5">
                  <Avatar name={m.nickname} seed={m.uid} size={24} />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm">{m.nickname}</div>
                    <div className="truncate font-mono text-[11px] text-subtle" title={m.uid}>
                      {m.uid}
                    </div>
                  </div>
                  <span className="shrink-0 text-[11px] text-subtle">{m.last_seen > 0 ? formatRelative(m.last_seen, now, lang) : t("members.lastSeenUnknown")}</span>
                </li>
              ))}
            </ul>
          )}
          {current.count > shown.length && <p className="text-xs text-subtle">{t("ss.pruneMore", { count: current.count - shown.length })}</p>}
        </section>
      )}
    </div>
  );
}

function RoleMenu({
  uid,
  groups,
  roles,
  stronger,
  isMe,
  nickname,
}: {
  uid: string;
  groups: number[];
  roles: Group[];
  stronger: boolean;
  isMe: boolean;
  nickname: string;
}) {
  const t = useT();
  const mine = useSession((s) => s.permissions);
  return (
    <Dropdown.Root>
      <Dropdown.Trigger asChild>
        <button
          type="button"
          aria-label={t("ss.editRoles", { name: nickname })}
          className="t inline-flex size-5 cursor-pointer items-center justify-center rounded-full border border-dashed border-line-strong text-subtle hover:border-accent hover:text-accent"
        >
          <Plus className="size-3" />
        </button>
      </Dropdown.Trigger>
      <Dropdown.Portal>
        <Dropdown.Content align="end" sideOffset={4} className={cn(menuContent, "w-56")}>
          {roles.map((g) => {
            const on = groups.includes(g.id);
            const blocked = stronger || !canAssignGroup(mine, g) || (isMe && g.id === ADMIN_GROUP && on);
            return (
              <Dropdown.CheckboxItem
                key={g.id}
                checked={on}
                disabled={blocked}
                title={blocked ? t("mod.roleBlocked") : undefined}
                onSelect={(e) => e.preventDefault()}
                onCheckedChange={(next) => void controller.attempt(controller.setMemberGroups(uid, toggleGroup(groups, g.id, next)))}
                className={cn(menuItem, "pl-7")}
              >
                <Dropdown.ItemIndicator className="absolute left-2 text-accent">
                  <Check className="size-4" />
                </Dropdown.ItemIndicator>
                <span className="size-2.5 shrink-0 rounded-full border border-line-strong" style={{ background: isHexColor(g.color) ? g.color : "transparent" }} />
                <span className="min-w-0 flex-1 truncate">{g.name}</span>
              </Dropdown.CheckboxItem>
            );
          })}
        </Dropdown.Content>
      </Dropdown.Portal>
    </Dropdown.Root>
  );
}

// --------------------------------------------------------------------- bans

function BansTab() {
  const t = useT();
  const lang = useLanguage();
  const [bans, setBans] = useState<BanEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now] = useState(() => Date.now());

  useEffect(() => {
    let alive = true;
    controller
      .listBans()
      .then((list) => alive && setBans(list))
      .catch((e: unknown) => alive && setError(describeRequestError(e)));
    return () => {
      alive = false;
    };
  }, []);

  const unban = async (id: number) => {
    if (await controller.attempt(controller.deleteBan(id))) {
      setBans((cur) => cur?.filter((b) => b.id !== id) ?? cur);
      useUi.getState().toast("success", t("ss.unbanned"));
    }
  };

  if (error) return <ErrorLine>{error}</ErrorLine>;
  if (!bans)
    return (
      <div className="flex justify-center py-8 text-subtle">
        <Spinner />
      </div>
    );
  if (bans.length === 0) return <EmptyState icon={<Ban className="size-5" />} title={t("ss.noBans")}>{t("ss.noBansHint")}</EmptyState>;
  return (
    <ul className="flex flex-col divide-y divide-line rounded-lg border border-line" aria-label={t("ss.bans")}>
      {bans.map((b) => (
        <li key={b.id} data-ban={b.nickname} className="flex flex-wrap items-center gap-3 px-3 py-2">
          <div className="min-w-0 flex-1 basis-48">
            <div className="flex items-center gap-2 text-sm font-medium">
              <span className="truncate">{b.nickname}</span>
              {b.ip && <span className="shrink-0 rounded bg-hover px-1 text-[10px] leading-4 font-medium text-subtle">{t("ss.ipBan")}</span>}
            </div>
            <div className="truncate text-xs text-muted">{b.reason ? b.reason : t("ss.noReason")}</div>
            <div className="text-[11px] text-subtle">
              {t("ss.bannedBy", { by: b.by })} · {b.expires_at ? t("ss.expires", { when: formatUntil(b.expires_at, now, lang) }) : t("ss.permanent")}
            </div>
          </div>
          <Button size="sm" onClick={() => void unban(b.id)}>
            {t("ss.unban")}
          </Button>
        </li>
      ))}
    </ul>
  );
}

// ------------------------------------------------------------------ invites

export function InvitesPanel() {
  const t = useT();
  const lang = useLanguage();
  const copy = useCopy();
  const defs = useSession((s) => s.groups);
  const mine = useSession((s) => s.permissions);
  const address = useSession((s) => s.address);
  const httpOrigin = useSession((s) => s.httpOrigin);
  const [invites, setInvites] = useState<Invite[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [uses, setUses] = useState("inf");
  const [expiry, setExpiry] = useState("1d");
  const [role, setRole] = useState("none");
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState<string | null>(null);
  const [now] = useState(() => Date.now());

  const assignable = useMemo(() => orderedGroups(defs).filter((g) => g.id !== MEMBER_GROUP && g.id !== ADMIN_GROUP && canAssignGroup(mine, g)), [defs, mine]);

  useEffect(() => {
    let alive = true;
    controller
      .listInvites()
      .then((list) => alive && setInvites(list))
      .catch((e: unknown) => alive && setError(describeRequestError(e)));
    return () => {
      alive = false;
    };
  }, []);

  const link = (code: string) =>
    buildInviteLink({ webOrigin: webOrigin(), serverOrigin: httpOrigin ?? webOrigin(), code });

  const create = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const { invite } = await controller.createInvite({
        max_uses: INVITE_USES.find((u) => u.id === uses)?.value ?? null,
        expires_in: INVITE_EXPIRY.find((x) => x.id === expiry)?.seconds ?? null,
        group: role === "none" ? null : Number(role),
      });
      setInvites((cur) => [invite, ...(cur ?? [])]);
    } catch (err) {
      setError(describeRequestError(err));
    }
    setBusy(false);
  };

  const remove = async (code: string) => {
    if (await controller.attempt(controller.deleteInvite(code))) setInvites((cur) => cur?.filter((i) => i.code !== code) ?? cur);
  };

  return (
    <div className="flex flex-col gap-4">
      <form onSubmit={(e) => void create(e)} className="grid gap-3 sm:grid-cols-3">
        <Field label={t("invite.maxUses")}>
          {(id) => (
            <Select id={id} value={uses} onValueChange={setUses} options={INVITE_USES.map((u) => ({ value: u.id, label: u.value === null ? t("invite.unlimited") : String(u.value) }))} />
          )}
        </Field>
        <Field label={t("invite.expiry")}>
          {(id) => <Select id={id} value={expiry} onValueChange={setExpiry} options={INVITE_EXPIRY.map((x) => ({ value: x.id, label: t(`invite.exp.${x.id}` as Key) }))} />}
        </Field>
        <Field label={t("invite.role")}>
          {(id) => (
            <Select id={id} value={role} onValueChange={setRole} options={[{ value: "none", label: t("invite.noRole") }, ...assignable.map((g) => ({ value: String(g.id), label: g.name }))]} />
          )}
        </Field>
        <div className="flex items-center gap-3 sm:col-span-3">
          <Button type="submit" variant="primary" busy={busy}>
            <UserPlus className="size-4" /> {t("invite.create")}
          </Button>
          <span className="min-w-0 truncate text-xs text-subtle">{t("invite.server", { address })}</span>
        </div>
      </form>
      <ErrorLine>{error}</ErrorLine>
      {invites === null ? (
        !error && (
          <div className="flex justify-center py-6 text-subtle">
            <Spinner />
          </div>
        )
      ) : invites.length === 0 ? (
        <EmptyState icon={<Link2 className="size-5" />} title={t("invite.none")}>
          {t("invite.noneHint")}
        </EmptyState>
      ) : (
        <ul aria-label={t("invite.list")} className="flex flex-col divide-y divide-line rounded-lg border border-line">
          {invites.map((i) => {
            const group = i.group != null ? defs[i.group] : undefined;
            const expired = i.expires_at != null && i.expires_at < now;
            return (
              <li key={i.code} data-invite={i.code} className="flex flex-wrap items-center gap-3 px-3 py-2">
                <div className="min-w-0 flex-1 basis-48">
                  <code className="block truncate font-mono text-xs text-fg">{i.code}</code>
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-subtle">
                    <span>{i.max_uses != null ? t("invite.usesOf", { uses: i.uses, max: i.max_uses }) : t("invite.usesCount", { uses: i.uses })}</span>
                    <span>·</span>
                    <span className={cn(expired && "text-danger")}>
                      {i.expires_at == null ? t("invite.neverExpires") : expired ? t("invite.expired") : t("ss.expires", { when: formatUntil(i.expires_at, now, lang) })}
                    </span>
                    {group && <RoleChip group={group} />}
                    <span>· {t("invite.by", { name: i.created_by })}</span>
                  </div>
                </div>
                <Button
                  size="sm"
                  aria-label={t("invite.copyLink")}
                  onClick={async () => {
                    if (await copy(link(i.code))) {
                      setCopied(i.code);
                      setTimeout(() => setCopied((c) => (c === i.code ? null : c)), 1500);
                    }
                  }}
                >
                  {copied === i.code ? <Check className="size-4" /> : <Copy className="size-4" />}
                  {copied === i.code ? t("toast.copied") : t("invite.copyLink")}
                </Button>
                <IconButton label={t("common.delete")} tone="danger" size="sm" onClick={() => void remove(i.code)}>
                  <Trash2 className="size-4" />
                </IconButton>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
