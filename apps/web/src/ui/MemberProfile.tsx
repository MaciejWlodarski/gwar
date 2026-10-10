import { Copy, Pencil } from "lucide-react";
import { useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { validNickname } from "../net/nickname";
import { controller, describeRequestError } from "../state/controller";
import { useSession, useUi } from "../state/stores";
import { ConnectBadge } from "./badges";
import { useMemberName, usePermission } from "./hooks";
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
  const vc = useSession((s) => s.kind === "vc");
  const mayRename = usePermission("member_nickname");
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
              <ConnectBadge handle={member?.connect} />
            </div>
            {member?.tag && <div className="font-mono text-xs text-muted">@{member.tag}</div>}
          </div>
        </div>
        {member?.connect && (
          <div className="flex items-center gap-2 text-xs text-muted">
            <ConnectBadge handle={member.connect} />
            {t("badge.connectProfile", { handle: member.connect })}
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
        {vc && (own || mayRename) && (
          <Button onClick={() => useUi.getState().openDialog({ kind: "nickname", uid })}>
            <Pencil className="size-4" /> {t("nickname.change")}
          </Button>
        )}
      </div>
    </Dialog>
  );
}
