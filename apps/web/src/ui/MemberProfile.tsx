import { Copy, Eye, Pencil } from "lucide-react";
import { useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { acceptHandle } from "../net/handle-pins";
import { validNickname } from "../net/nickname";
import { controller, describeRequestError } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { ConnectBadge, IdentityLine } from "./badges";
import { useMemberName, useMyWho, usePermission, useWhoOf } from "./hooks";
import { canActOn } from "../lib/permissions";

const NO_GROUPS: readonly number[] = [];
import { useConnectHandle } from "./useConnectHandle";
import { isDesktop } from "../platform";
import { useAccount } from "../state/account";
import { shortUid } from "../connect/ts-list";
import { Avatar, Button, Dialog, Field, Input } from "./kit";

export function NicknameDialog({ uid }: { uid: string }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const current = useMemberName(uid, "");
  const [nickname, setNickname] = useState(current);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const save = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy) return;
    const clean = validNickname(nickname);
    if (!clean) {
      setError(t("nickname.invalid"));
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await controller.setMemberNickname(uid, clean);
      close();
    } catch (e) {
      setError(describeRequestError(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("nickname.change")}
      description={t("nickname.serverHint")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={!validNickname(nickname)} onClick={() => void save()}>
            {t("common.save")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void save(e)} className="flex flex-col gap-3">
        <Field label={t("connect.nickname")}>
          {(id) => <Input id={id} value={nickname} onChange={(e) => setNickname(e.target.value)} autoFocus />}
        </Field>
        {error && <p role="alert" className="text-sm text-danger">{error}</p>}
      </form>
    </Dialog>
  );
}

/** Profile by uid: names, tags and verified account handles stay live while open. */
export function MemberProfileDialog({ uid, fallback }: { uid: string; fallback: string }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const member = useSession((s) => s.members[uid]);
  const nickname = useMemberName(uid, fallback);
  const own = useSession((s) => s.me?.uid === uid);
  const onGwar = useSession((s) => s.kind === "gwar");
  const canRename = usePermission("manage_members");
  const canViewAs = usePermission("manage_roles");
  const me = useMyWho();
  const groupDefs = useSession((s) => s.groups);
  const target = useWhoOf(uid, member?.groups ?? NO_GROUPS);
  // Someone else's nickname only if they rank below me (docs/roles.md).
  const mayRename = canRename && canActOn(me, target, groupDefs);
  const connect = useConnectHandle(member);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(uid);
      useUi.getState().toast("success", t("toast.copied"));
    } catch {
      useUi.getState().toast("error", t("toast.copyFailed"));
    }
  };
  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("member.profile")} width="max-w-sm">
      <div className="flex flex-col gap-4">
        <div className="flex items-center gap-3">
          <Avatar name={nickname} seed={uid} size={40} />
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className="truncate font-semibold">{nickname}</span>
              <ConnectBadge member={member} />
            </div>
            <IdentityLine member={member} tag={member?.tag} className="block" />
          </div>
        </div>
        {connect?.status === "changed" && (
          <div className="flex flex-col gap-2 rounded-md bg-raised p-3 text-sm text-muted">
            <p>{t("member.handleChanged", { handle: connect.handle })}</p>
            <Button
              className="self-start"
              onClick={() => void acceptHandle(connect.handle, uid).then((saved) => saved || useUi.getState().toast("error", t("member.acceptNotSaved")))}
            >
              {t("member.acceptHandle")}
            </Button>
          </div>
        )}
        <Field label={t("member.uid")}>
          {(id) => (
            <div className="flex gap-2">
              <Input id={id} readOnly value={uid} className="min-w-0 font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />
              <Button onClick={() => void copy()} aria-label={t("common.copy")}>
                <Copy className="size-4" />
              </Button>
            </div>
          )}
        </Field>
        {onGwar && (own || mayRename) && (
          <Button onClick={() => useUi.getState().openDialog({ kind: "nickname", uid })}>
            <Pencil className="size-4" /> {t("nickname.change")}
          </Button>
        )}
        {onGwar && canViewAs && (
          <Button onClick={() => useUi.getState().openDialog({ kind: "viewAs", uid })}>
            <Eye className="size-4" /> {t("viewAs.openMember")}
          </Button>
        )}
        {onGwar && <TeamspeakLinks uid={uid} linked={member?.teamspeak ?? NO_LINKS} own={own} mayUnlink={own || mayRename} />}
      </div>
    </Dialog>
  );
}

const NO_LINKS: string[] = [];

/** The member's linked TeamSpeak identities (docs/teamspeak-import.md); one's own can also be claimed here (the desktop's, or the account's in the browser). */
function TeamspeakLinks({ uid, linked, own, mayUnlink }: { uid: string; linked: string[]; own: boolean; mayUnlink: boolean }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [confirming, setConfirming] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // In the browser the identities are the account's, so it needs the vault key.
  const hasVault = useAccount((s) => !!s.account?.hasVaultKey);
  const canClaim = own && (isDesktop() || hasVault);
  if (linked.length === 0 && !canClaim) return null;
  const unlink = async (teamspeak: string) => {
    setBusy(true);
    try {
      await controller.unlinkTeamspeak(uid, teamspeak);
      toast("success", t("tsClaim.unlinked"));
    } catch (e) {
      toast("error", describeRequestError(e));
    } finally {
      setBusy(false);
      setConfirming(null);
    }
  };
  const claim = async () => {
    setBusy(true);
    try {
      const result = await controller.claimTeamspeak();
      if (!result) toast("success", t("tsClaim.none"));
      else toast(result.linked === result.of ? "success" : "error", t(result.linked === result.of ? "tsClaim.done" : "tsClaim.failed"));
    } catch (e) {
      toast("error", describeRequestError(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <section aria-label={t("tsClaim.linked")} className="flex flex-col gap-2">
      {linked.length > 0 && (
        <>
          <h3 className="text-xs font-semibold uppercase text-muted">{t("tsClaim.linked")}</h3>
          <ul className="flex flex-col gap-1">
            {linked.map((teamspeak) => (
              <li key={teamspeak} className="flex items-center gap-2 rounded-md bg-raised px-3 py-2">
                <span className="min-w-0 flex-1 truncate font-mono text-xs">{shortUid(teamspeak.slice(3))}</span>
                {mayUnlink && confirming !== teamspeak && (
                  <Button size="sm" variant="ghost" disabled={busy} onClick={() => setConfirming(teamspeak)}>
                    {t("tsClaim.unlink")}
                  </Button>
                )}
                {mayUnlink && confirming === teamspeak && (
                  <Button size="sm" variant="danger" busy={busy} onClick={() => void unlink(teamspeak)}>
                    {t("tsClaim.unlink")}?
                  </Button>
                )}
              </li>
            ))}
          </ul>
          {confirming && <p className="text-xs text-muted">{t("tsClaim.unlinkConfirm")}</p>}
        </>
      )}
      {canClaim && (
        <Button busy={busy} onClick={() => void claim()}>
          {t("tsClaim.check")}
        </Button>
      )}
    </section>
  );
}
