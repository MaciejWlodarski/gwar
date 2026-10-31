import { useState } from "react";
import { useT } from "../i18n";
import type { InviteTarget } from "../net/invite";
import { displayText } from "../lib/text";
import { controller } from "../state/controller";
import { useConnectUi, useUi } from "../state/stores";
import { Button, Dialog } from "./kit";

export function InviteDetails({ target }: { target: InviteTarget }) {
  const t = useT();
  return <div className="flex flex-col gap-3 text-sm">
    <p className="text-muted">{t("connect.inviteReviewHint")}</p>
    <dl className="flex flex-col gap-2">
      {target.name && <><dt className="text-muted">{t("connect.inviteName")}</dt><dd className="break-words">{displayText(target.name)}</dd></>}
      <dt className="text-muted">{t("connect.address")}</dt><dd className="break-all">{displayText(target.server ?? "")}</dd>
      <dt className="text-muted">{t("connect.inviteKey")}</dt><dd className="break-all font-mono text-xs">{target.serverKey}</dd>
    </dl>
  </div>;
}

/** A running connection is left only after the person chooses to review the new destination. */
export function InviteReviewDialog({ target }: { target: InviteTarget }) {
  const t = useT();
  const close = useUi(s => s.closeDialog);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  const review = async () => {
    setBusy(true);
    setError(false);
    try {
      await controller.disconnect();
      const dialog = useUi.getState().dialog;
      if (dialog.kind !== "inviteReview" || dialog.target !== target) return;
      useConnectUi.getState().set({ invite: target, inviteLaunch: true });
      useUi.getState().setSection("servers");
      close();
    } catch { setError(true); }
    finally { setBusy(false); }
  };
  return <Dialog open title={t("connect.inviteDetails")} onOpenChange={o => !o && !busy && close()} footer={<><Button disabled={busy} onClick={close}>{t("common.cancel")}</Button><Button busy={busy} onClick={() => void review()}>{t("connect.reviewInvite")}</Button></>}>
    <InviteDetails target={target} />
    <p className="mt-3 text-sm text-muted">{t("connect.leaveToReview")}</p>
    {error && <p role="alert" className="mt-3 text-sm text-danger">{t("connect.reviewFailed")}</p>}
  </Dialog>;
}
