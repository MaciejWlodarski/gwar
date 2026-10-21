import { MemberProfileDialog, NicknameDialog } from "./MemberProfile";
import { sameServer } from "../lib/server-key";
import { Check, Copy } from "lucide-react";
import { useEffect, useMemo, useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { RequestError } from "../net/connection";
import type { Channel } from "../proto/Channel";
import type { ChannelAccess } from "../proto/ChannelAccess";
import { AccessPicker } from "./ChannelAccess";
import { ChannelPermissionsDialog } from "./ChannelPermissions";
import { BugReportDialog } from "./BugReport";
import { rebaseAccess, sameAccess } from "../lib/access";
import { bookmarkParams, controller, describeJoinError, describeRequestError } from "../state/controller";
import { sortedChannels } from "../state/reducer";
import { isDesktop } from "../platform";
import { newId, useSettings, type ServerKind } from "../state/settings";
import { useSession, useUi, type DialogState } from "../state/stores";
import { Button, Dialog, Field, Input, Segmented, Select, Textarea, Switch } from "./kit";
import { SettingsDialog } from "./Settings";
import { HearersDialog, ReadersDialog } from "./Readers";
import { BanDialog } from "./BanDialog";
import { ChannelInfoDialog, PokeDialog } from "./TeamspeakDialogs";
import { RemoveMemberDialog } from "./RemoveMemberDialog";
import { Lightbox } from "./Attachments";
import { InvitesDialog, ServerSettingsDialog } from "./ServerSettings";
import { FirstRunDialog } from "./TeamspeakFound";
import { ClaimDialog } from "./TeamspeakClaim";
import { ViewAsDialog } from "./ViewAsDialog";
import { IdentityPicker } from "./IdentityPicker";
import { useMemberName, useMyWho, useOwnerView, useTsIdentities } from "./hooks";
import { canGiveGroup, orderedGroups } from "../lib/permissions";
import { resolveIdentity } from "../connect/ts-list";

export function Dialogs() {
  const dialog = useUi((s) => s.dialog);
  switch (dialog.kind) {
    case "none":
      return null;
    case "settings":
      return <SettingsDialog tab={dialog.tab} />;
    case "bugReport":
      return <BugReportDialog />;
    case "addServer":
      return <AddServerDialog editId={dialog.editId} />;
    case "channelEdit":
      return dialog.mode === "create" ? <ChannelDialog mode="create" parent={dialog.parent} /> : <ChannelDialog mode="edit" channel={dialog.channel} />;
    case "channelPassword":
      return <ChannelPasswordDialog channel={dialog.channel} />;
    case "channelInfo":
      return <ChannelInfoDialog channel={dialog.channel} />;
    case "poke":
      return <PokeDialog person={dialog.person} />;
    case "channelPermissions":
      return <ChannelPermissionsDialog channel={dialog.channel} />;
    case "serverSettings":
      return <ServerSettingsDialog tab={dialog.tab} />;
    case "invites":
      return <InvitesDialog />;
    case "nickname":
      return <NicknameDialog key={dialog.uid} uid={dialog.uid} />;
    case "memberProfile":
      return <MemberProfileDialog uid={dialog.uid} fallback={dialog.fallback} />;
    case "ban":
      return <BanDialog person={dialog.person} back={dialog.back} />;
    case "removeMember":
      return <RemoveMemberDialog person={dialog.person} back={dialog.back} />;
    case "lightbox":
      return <Lightbox attachment={dialog.attachment} verified={dialog.verified} />;
    case "redeem":
      return <RedeemDialog />;
    case "createToken":
      return <CreateTokenDialog />;
    case "serverKey":
      return <ServerKeyDialog />;
    case "readers":
      return <ReadersDialog thread={dialog.thread} />;
    case "hearers":
      return <HearersDialog channel={dialog.channel} />;
    case "confirm":
      return <ConfirmDialog {...dialog} />;
    case "tsFirstRun":
      return <FirstRunDialog found={dialog.found} resolve={dialog.resolve} />;
    case "viewAs":
      return <ViewAsDialog role={dialog.role} uid={dialog.uid} back={dialog.back} />;
    case "tsClaim":
      return <ClaimDialog identities={dialog.identities} resolve={dialog.resolve} />;
  }
}

function useClose() {
  return useUi((s) => s.closeDialog);
}

// ------------------------------------------------------------------ confirm

function ConfirmDialog({ title, memberTitle, body, confirmLabel, danger, serverIdentity, onConfirm }: Extract<DialogState, { kind: "confirm" }>) {
  const t = useT();
  const close = useClose();
  const nickname = useMemberName(memberTitle?.uid ?? "", memberTitle?.fallback ?? "");
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={memberTitle ? t(memberTitle.key, { name: nickname }) : title}
      description={serverIdentity ? undefined : body}
      width={serverIdentity ? "max-w-xl" : "max-w-sm"}
      footer={
        <div className={serverIdentity ? "flex w-full min-w-0 flex-col-reverse gap-2 sm:flex-row sm:justify-end" : "flex gap-2"}>
          <Button onClick={close} autoFocus={serverIdentity}>{t("common.cancel")}</Button>
          <Button
            variant={danger ? "danger" : "primary"}
            className={serverIdentity ? "h-auto min-h-9 max-w-full whitespace-normal py-2 text-center" : undefined}
            autoFocus={!serverIdentity}
            onClick={() => {
              close();
              onConfirm();
            }}
          >
            {confirmLabel}
          </Button>
        </div>
      }
    >
      {serverIdentity ? <p className="whitespace-pre-line wrap-anywhere text-sm text-muted">{body}</p> : <span className="sr-only">{body}</span>}
    </Dialog>
  );
}

// --------------------------------------------------------------- add server

function AddServerDialog({ editId }: { editId?: string }) {
  const t = useT();
  const close = useClose();
  const existing = useSettings((s) => s.bookmarks.find((b) => b.id === editId));
  const desktop = isDesktop();
  const [kind, setKind] = useState<ServerKind>(existing?.kind ?? "gwar");
  const [name, setName] = useState(existing?.name ?? "");
  const [address, setAddress] = useState(existing?.address ?? "");
  const [password, setPassword] = useState(existing?.password ?? "");
  const [insecureLan, setInsecureLan] = useState(existing?.insecureLan ?? false);
  const tsList = useTsIdentities();
  const [identity, setIdentity] = useState<string | null>(existing?.identity ?? null);
  const [channel, setChannel] = useState(existing?.channel ?? "");
  const [channelPassword, setChannelPassword] = useState(existing?.channelPassword ?? "");
  const pickIdentity = kind === "teamspeak" && !!tsList && tsList.identities.length >= 2;

  const save = (connect: boolean) => (e?: FormEvent) => {
    e?.preventDefault();
    if (!address.trim()) return;
    const bookmark = {
      id: existing?.id ?? newId(),
      name: name.trim() || address.trim(),
      kind,
      insecureLan: desktop && kind === "gwar" && insecureLan,
      address: address.trim(),
      password: password || undefined,
      // Without a choice to make, a remembered identity stays as it was.
      identity: kind === "teamspeak" ? (pickIdentity ? (resolveIdentity(tsList, identity) ?? undefined) : existing?.identity) : undefined,
      channel: kind === "teamspeak" ? channel.trim() || undefined : undefined,
      channelPassword: kind === "teamspeak" && channel.trim() ? channelPassword || undefined : undefined,
    };
    useSettings.getState().saveBookmark(bookmark);
    close();
    if (connect) {
      // A server saved before keeps its bookmark (and password): connect with what was stored.
      const stored = useSettings.getState().bookmarks.find((b) => b.id === bookmark.id) ?? useSettings.getState().bookmarks.find((b) => sameServer(b, bookmark)) ?? bookmark;
      void controller.connectInteractive(bookmarkParams(stored), { remember: false });
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={existing ? t("addServer.editTitle") : t("addServer.title")}
      description={t("addServer.description")}
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button onClick={() => save(false)()} disabled={!address.trim()}>
            {t("common.save")}
          </Button>
          <Button variant="primary" onClick={() => save(true)()} disabled={!address.trim()}>
            {t("addServer.saveConnect")}
          </Button>
        </>
      }
    >
      <form onSubmit={save(true)} className="flex flex-col gap-3">
        <Field label={t("addServer.name")} hint={t("addServer.nameHint")}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={48} />}
        </Field>
        <Segmented<ServerKind>
          label={t("connect.kind")}
          value={kind}
          onChange={setKind}
          options={[
            { value: "gwar", label: t("connect.kindVc") },
            { value: "teamspeak", label: t("connect.kindTs"), disabled: !desktop && kind !== "teamspeak", title: desktop ? undefined : t("connect.tsWebOnly") },
          ]}
        />
        {!desktop && <p className="-mt-1 text-xs text-subtle">{t("connect.tsWebOnly")}</p>}
        <Field label={t("connect.address")} hint={kind === "teamspeak" ? t("connect.tsAddressHint") : t("connect.addressHint")}>
          {(id) => (
            <Input id={id} value={address} onChange={(e) => setAddress(e.target.value)} placeholder={kind === "teamspeak" ? "ts.example.com" : "voice.example.com"} maxLength={255} spellCheck={false} autoCapitalize="none" autoFocus />
          )}
        </Field>

        <Field label={t("addServer.password")} hint={kind === "teamspeak" ? t("addServer.tsPasswordHint") : t("addServer.passwordHint")}>
          {(id) => <Input id={id} type="password" value={password} onChange={(e) => setPassword(e.target.value)} maxLength={128} autoComplete="off" />}
        </Field>
        {desktop && kind === "gwar" && <div className="flex flex-col gap-2">
          <Switch checked={insecureLan} onCheckedChange={setInsecureLan} label={t("security.insecureLan")} />
          {insecureLan && <p className="text-sm text-danger">{t("security.insecureWarning")}</p>}
        </div>}
        {kind === "teamspeak" && (
          <>
            <Field label={t("addServer.tsChannel")} hint={t("addServer.tsChannelHint")}>
              {(id) => <Input id={id} value={channel} onChange={(e) => setChannel(e.target.value)} placeholder={t("addServer.tsChannelPlaceholder")} maxLength={255} spellCheck={false} />}
            </Field>
            {channel.trim() && (
              <Field label={t("addServer.tsChannelPassword")} hint={t("addServer.tsChannelPasswordHint")}>
                {(id) => <Input id={id} type="password" value={channelPassword} onChange={(e) => setChannelPassword(e.target.value)} maxLength={128} autoComplete="off" />}
              </Field>
            )}
          </>
        )}
        {pickIdentity && <IdentityPicker list={tsList} value={identity} onChange={setIdentity} />}
        <button type="submit" className="hidden" />
      </form>
    </Dialog>
  );
}

// ------------------------------------------------------------------ channel

function descendants(channels: Channel[], root: number): Set<number> {
  const out = new Set<number>([root]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const c of channels) {
      if (c.parent !== null && out.has(c.parent) && !out.has(c.id)) {
        out.add(c.id);
        grew = true;
      }
    }
  }
  return out;
}

const NONE = "__none";

function ChannelDialog(props: { mode: "create"; parent: number | null } | { mode: "edit"; channel: number }) {
  const t = useT();
  const close = useClose();
  const channels = useSession((s) => s.channels);
  const existing = props.mode === "edit" ? channels[props.channel] : undefined;
  const [name, setName] = useState(existing?.name ?? "");
  const [topic, setTopic] = useState(existing?.topic ?? "");
  const [password, setPassword] = useState("");
  const [removePassword, setRemovePassword] = useState(false);
  const [maxClients, setMaxClients] = useState(existing?.max_clients ? String(existing.max_clients) : "");
  const [parent, setParent] = useState<string>(
    props.mode === "create" ? String(props.parent ?? NONE) : String(existing?.parent ?? NONE),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Encryption is chosen once, when the channel is created; on Gwar servers only.
  const serverKey = controller.messageServerKey;
  const offerEncryption = useSession((s) => s.kind === "gwar") && props.mode === "create";
  const [encrypted, setEncrypted] = useState(() => useSettings.getState().channelEncryption[serverKey] ?? false);
  const [history, setHistory] = useState(true);
  // A private channel is hidden from everyone its access does not admit (fixed at creation).
  const me = useSession((s) => s.me?.uid ?? "");
  const [privateChannel, setPrivateChannel] = useState(false);
  const isPrivate = props.mode === "create" ? privateChannel : !!existing?.private;
  const [access, setAccess] = useState<ChannelAccess>(() => existing?.access ?? { roles: [], users: me ? [me] : [] });
  // What the dialog opened with: saving sends only the changes made here, on top of the current access.
  const [openedAccess] = useState(access);
  // An encrypted private channel (6c): its members are a list signed by the owner, a confirmer or a
  // manager, which only they can create or edit.
  const ownerView = useOwnerView();
  const authority = !!ownerView && !!ownerView.anchor_seq && (ownerView.owner === me || ownerView.confirmers.includes(me) || ownerView.managers.includes(me));
  const encryptedPrivate = props.mode === "create" ? offerEncryption && encrypted && privateChannel : !!existing?.private && !!existing?.encrypted;
  const [signedMembers, setSignedMembers] = useState(() => (existing && encryptedPrivate ? controller.privateMembers(existing.id) : null));
  const [members, setMembers] = useState<ChannelAccess>(() => ({ roles: [], users: signedMembers?.members ?? (me ? [me] : []) }));
  // Opened before this device holds the list: it is shown (and can be edited) once it does.
  const editingId = props.mode === "edit" && existing && encryptedPrivate ? existing.id : null;
  useEffect(() => {
    if (editingId === null || signedMembers) return;
    const timer = setInterval(() => {
      const view = controller.privateMembers(editingId);
      if (!view) return;
      setSignedMembers(view);
      setMembers({ roles: [], users: view.members });
    }, 1_000);
    return () => clearInterval(timer);
  }, [editingId, signedMembers]);
  const signerName = useMemberName(signedMembers?.signer_uid ?? "", (signedMembers?.signer_uid ?? "").slice(0, 8));
  // An encrypted channel has no password: its messages are readable only by its group anyway.
  const sealedChannel = (offerEncryption ? encrypted : !!existing?.encrypted) || isPrivate;

  useEffect(() => {
    if (props.mode === "edit" && !existing) close();
  }, [props.mode, existing, close]);

  const parentOptions = useMemo(() => {
    const all = sortedChannels(Object.values(channels));
    const blocked = props.mode === "edit" ? descendants(all, props.channel) : new Set<number>();
    return [
      { value: NONE, label: t("channel.noParent") },
      // A private channel has no child channels.
      ...all.filter((c) => !blocked.has(c.id) && !c.private).map((c) => ({ value: String(c.id), label: c.name })),
    ];
  }, [channels, props, t]);

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (!name.trim() || busy) return;
    const max = maxClients.trim() ? Number(maxClients) : null;
    if (max !== null && (!Number.isInteger(max) || max < 1 || max > 1000)) {
      setError(t("channel.maxInvalid"));
      return;
    }
    setBusy(true);
    setError(null);
    const parentId = parent === NONE ? null : Number(parent);
    try {
      if (props.mode === "create") {
        const payload = {
          name: name.trim(),
          parent: parentId,
          topic: topic.trim() || null,
          password: sealedChannel ? null : password || null,
          max_clients: max,
          ...(offerEncryption && encrypted ? { encrypted: true, history } : {}),
          ...(privateChannel ? { private: true, ...(encryptedPrivate ? {} : { access }) } : {}),
        };
        await (encryptedPrivate ? controller.createChannel(payload, members.users) : controller.createChannel(payload));
        if (offerEncryption) useSettings.getState().setChannelEncryption(serverKey, encrypted);
      } else if (existing) {
        const parentChanged = parentId !== existing.parent;
        await controller.updateChannel({
          channel: existing.id,
          // Only text edited here: the shown name and topic may be cleaned copies of the server's.
          ...(name !== (existing.name ?? "") ? { name: name.trim() } : {}),
          ...(topic !== (existing.topic ?? "") ? { topic: topic.trim() } : {}),
          // "" removes the password; absent keeps it.
          ...(sealedChannel ? {} : removePassword ? { password: "" } : password ? { password } : {}),
          // 0 removes the limit (protocol rule for channel.update).
          ...(max !== (existing.max_clients ?? null) ? { max_clients: max ?? 0 } : {}),
          ...(parentChanged ? (parentId === null ? { move_to_root: true } : { parent: parentId }) : {}),
          ...(existing.private && !encryptedPrivate && !sameAccess(openedAccess, access) ? { access: rebaseAccess(openedAccess, access, existing.access) } : {}),
        });
        // Only the changes made here, applied to the list as it is when signing (others' changes stay).
        const before = signedMembers?.members ?? [];
        const added = members.users.filter((u) => !before.includes(u));
        const removed = before.filter((u) => !members.users.includes(u));
        if (encryptedPrivate && signedMembers && (added.length || removed.length)) {
          await controller.setPrivateMembers(existing.id, added, removed);
        }
      }
      close();
    } catch (err) {
      setError(describeRequestError(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={props.mode === "create" ? t("channel.createTitle") : t("channel.editTitle")}
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={!name.trim() || (props.mode === "create" && encryptedPrivate && !authority)} onClick={() => void submit()}>
            {props.mode === "create" ? t("common.create") : t("common.save")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
        <Field label={t("channel.name")}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} maxLength={64} autoFocus required />}
        </Field>
        <Field label={t("channel.topic")}>
          {(id) => <Textarea id={id} rows={2} value={topic} onChange={(e) => setTopic(e.target.value)} maxLength={255} />}
        </Field>
        <Field label={t("channel.parent")}>
          {(id) => <Select id={id} value={parent} onValueChange={setParent} options={parentOptions} />}
        </Field>
        <div className="grid grid-cols-2 gap-3">
          {!sealedChannel && <Field
            label={t("channel.password")}
            hint={existing?.has_password ? (removePassword ? t("channel.passwordWillRemove") : t("channel.passwordKeep")) : t("channel.passwordNone")}
          >
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
              />
            )}
          </Field>}
          <Field label={t("channel.maxClients")} hint={t("channel.maxHint")}>
            {(id) => <Input id={id} inputMode="numeric" value={maxClients} onChange={(e) => setMaxClients(e.target.value.replace(/\D/g, ""))} maxLength={4} />}
          </Field>
        </div>
        {offerEncryption && (
          <Switch checked={privateChannel} onCheckedChange={setPrivateChannel} label={t("channel.private")} description={t("channel.privateHint")} />
        )}
        {isPrivate && !encryptedPrivate && <AccessPicker value={access} onChange={setAccess} />}
        {encryptedPrivate && (
          <>
            <AccessPicker value={members} onChange={setMembers} membersOnly disabled={!authority || (editingId !== null && !signedMembers)} />
            {editingId !== null && !signedMembers && <p className="text-xs text-subtle">{t("channel.membersLoading")}</p>}
            {signedMembers && (
              <p className="text-xs text-subtle">{t("channel.membersSigned", { name: signerName, date: new Date(signedMembers.signed_at).toLocaleString() })}</p>
            )}
            {!authority && <p className="text-xs text-subtle">{t("channel.membersAuthority")}</p>}
          </>
        )}
        {offerEncryption && (
          <div className="flex flex-col gap-1">
            <Switch checked={encrypted} onCheckedChange={setEncrypted} label={t("channel.encrypted")} description={t("channel.encryptedHint")} />
            {encrypted && <Switch checked={history} onCheckedChange={setHistory} label={t("channel.history")} description={t("channel.historyHint")} />}
            <p className="text-xs text-subtle">{t("channel.encryptionPermanent")}</p>
          </div>
        )}
        {existing?.has_password && !sealedChannel && (
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
            {t("channel.removePassword")}
          </label>
        )}
        {error && (
          <p role="alert" className="rounded-md bg-danger-soft px-3 py-2 text-sm text-danger">
            {error}
          </p>
        )}
        <button type="submit" className="hidden" />
      </form>
    </Dialog>
  );
}

function ChannelPasswordDialog({ channel }: { channel: number }) {
  const t = useT();
  const close = useClose();
  const name = useSession((s) => s.channels[channel]?.name ?? "");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy || !password) return;
    setBusy(true);
    setError(null);
    try {
      // An answer for an ended session must not close what is open now. After a resync this dialog
      // is still open: let the password be sent again.
      if (!(await controller.joinChannel(channel, password))) {
        setBusy(false);
        return;
      }
      close();
      useUi.getState().setDrawer(false);
    } catch (err) {
      setError(err instanceof RequestError && err.code === "wrong_password" ? t("channelPassword.wrong") : describeJoinError(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("channelPassword.title", { name })}
      description={t("channelPassword.description")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={!password} onClick={() => void submit()}>
            {t("tree.join")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)}>
        <Field label={t("channel.password")} error={error}>
          {(id) => <Input id={id} type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoFocus autoComplete="off" />}
        </Field>
      </form>
    </Dialog>
  );
}

// ------------------------------------------------------------------- server

function RedeemDialog() {
  const t = useT();
  const close = useClose();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy || !token.trim()) return;
    setBusy(true);
    setError(null);
    try {
      const r = await controller.redeemToken(token.trim());
      const groups = useSession.getState().groups;
      const names = r.groups.map((g) => groups[g]?.name).filter(Boolean).join(", ");
      useUi.getState().toast("success", t("redeem.success", { groups: names }));
      close();
    } catch (err) {
      setError(err instanceof RequestError && err.code === "not_found" ? t("redeem.invalid") : describeRequestError(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("redeem.title")}
      description={t("redeem.description")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={!token.trim()} onClick={() => void submit()}>
            {t("redeem.submit")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)}>
        <Field label={t("redeem.token")} error={error}>
          {(id) => <Input id={id} value={token} onChange={(e) => setToken(e.target.value)} className="font-mono" autoFocus spellCheck={false} autoComplete="off" />}
        </Field>
      </form>
    </Dialog>
  );
}

/** The verified key of the connected server, to compare with its owner. */
function ServerKeyDialog() {
  const t = useT();
  const close = useClose();
  const serverKey = useSession((s) => s.serverKey);
  const endpoint = useSession((s) => s.endpoint);
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    if (!serverKey) return;
    try {
      await navigator.clipboard.writeText(serverKey);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      useUi.getState().toast("error", t("toast.copyFailed"));
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("security.fingerprint")}
      description={t("security.fingerprintHelp")}
      width="max-w-sm"
      footer={<Button onClick={close}>{t("common.close")}</Button>}
    >
      <div className="flex flex-col gap-2">
        <p className="text-xs text-subtle wrap-anywhere">{endpoint}</p>
        <div className="flex items-center gap-2 rounded-lg border border-line-strong bg-side p-2">
          <code data-testid="server-fingerprint" className="min-w-0 flex-1 font-mono text-xs break-all select-all">{serverKey}</code>
          <Button size="sm" onClick={() => void copy()}>
            {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
            {copied ? t("toast.copied") : t("common.copy")}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

function CreateTokenDialog() {
  const t = useT();
  const close = useClose();
  const groups = useSession((s) => s.groups);
  const me = useMyWho();
  // A token can only grant a role below its creator's rank.
  const eligible = useMemo(() => orderedGroups(groups).filter((g) => canGiveGroup(me, g, groups)), [groups, me]);
  const [group, setGroup] = useState(String(eligible.find((g) => g.permissions.length > 0)?.id ?? eligible[0]?.id ?? ""));
  const [token, setToken] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      setToken((await controller.createToken(Number(group))).token);
    } catch (err) {
      setError(describeRequestError(err));
    }
    setBusy(false);
  };

  const copy = async () => {
    if (!token) return;
    try {
      await navigator.clipboard.writeText(token);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      useUi.getState().toast("error", t("toast.copyFailed"));
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("token.title")}
      description={t("token.description")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.close")}</Button>
          {!token && (
            <Button variant="primary" busy={busy} disabled={!group} onClick={() => void create()}>
              {t("token.create")}
            </Button>
          )}
        </>
      }
    >
      <div className="flex flex-col gap-3">
        {!token ? (
          <Field label={t("token.group")} error={error}>
            {(id) => <Select id={id} value={group} onValueChange={setGroup} options={eligible.map((g) => ({ value: String(g.id), label: g.name }))} />}
          </Field>
        ) : (
          <>
            <p className="text-sm text-muted">{t("token.created")}</p>
            <div className="flex items-center gap-2 rounded-lg border border-line-strong bg-side p-2">
              <code className="min-w-0 flex-1 font-mono text-xs break-all select-all">{token}</code>
              <Button size="sm" onClick={() => void copy()}>
                {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
                {copied ? t("toast.copied") : t("common.copy")}
              </Button>
            </div>
            <p className="text-xs text-subtle">{t("token.once")}</p>
          </>
        )}
      </div>
    </Dialog>
  );
}
