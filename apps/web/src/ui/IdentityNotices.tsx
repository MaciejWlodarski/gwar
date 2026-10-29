import { ShieldAlert, X } from "lucide-react";
import { useEffect, useState } from "react";
import { useLanguage, useT, countKey } from "../i18n";
import { certificateWarning } from "../connect/expiry";
import { MEMBER_GROUP } from "../lib/permissions";
import { identityNudgeDue } from "../lib/onboarding";
import { accountActions, useAccount } from "../state/account";
import { exportIdentityBackup } from "../state/identity-backup";
import { useSettings } from "../state/settings";
import { useSession, useUi } from "../state/stores";
import { PasswordDialog } from "./AccountSettings";
import { Button, Dialog } from "./kit";

function useNoticeTime() {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const update = () => setNow(Date.now());
    const timer = setInterval(update, 30_000);
    window.addEventListener("focus", update);
    document.addEventListener("visibilitychange", update);
    return () => { clearInterval(timer); window.removeEventListener("focus", update); document.removeEventListener("visibilitychange", update); };
  }, []);
  return now;
}

/** Always visible from the connect screen and the server; never depends on toast timing. */
export function IdentityNotices() {
  const account = useAccount((s) => s.account);
  // A new or renewed device starts a fresh clock and closes any old password prompt.
  return <IdentityNoticesContent key={account ? `${account.deviceKey}:${account.expiresAt}` : "local"} />;
}

function IdentityNoticesContent() {
  const t = useT();
  const language = useLanguage();
  const { account, certChecked, loaded } = useAccount();
  const now = useNoticeTime();
  const onboarding = useSettings((s) => s.onboarding);
  const hasRole = useSession((s) => s.kind === "gwar" && !!s.me && !!s.members[s.me.uid]?.groups.some((id) => id !== MEMBER_GROUP));
  const openSettings = useUi((s) => s.openSettings);
  const [renewing, setRenewing] = useState(false);
  const [confirmingBackup, setConfirmingBackup] = useState(false);
  const [exporting, setExporting] = useState(false);
  useEffect(() => {
    if (hasRole && onboarding && !onboarding.roleReceived) useSettings.getState().setOnboarding({ roleReceived: true });
  }, [hasRole, onboarding]);
  const warning = account ? certificateWarning(account.expiresAt, now, certChecked) : { level: "none" as const };
  const nudge = loaded && identityNudgeDue(onboarding, !!account, now, navigator.userAgent);
  const backup = async () => {
    setExporting(true);
    try { await exportIdentityBackup(); setConfirmingBackup(false); }
    catch { useUi.getState().toast("error", t("identity.exportFailed")); }
    finally { setExporting(false); }
  };
  return (
    <>
      {warning.level !== "none" && (
        <div data-testid="expiry-banner" role="alert" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-line bg-raised px-4 py-3 text-sm">
          <ShieldAlert className="size-4 shrink-0 text-warn" />
          <p className="min-w-0 flex-1 basis-56">{warning.level === "expired" ? t("account.signInExpired") : t(countKey(language, "account.signInDays", warning.daysLeft), { count: warning.daysLeft })}</p>
          <Button size="sm" onClick={() => setRenewing(true)}>{t("account.signInAgain")}</Button>
        </div>
      )}
      {nudge && (
        <div data-testid="identity-banner" role="status" className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-line bg-raised px-4 py-3 text-sm">
          <ShieldAlert className="size-4 shrink-0 text-warn" />
          <p className="min-w-0 flex-1 basis-56">{t("identity.keepIdentity")}</p>
          <div className="flex shrink-0 items-center gap-2">
            <Button size="sm" onClick={() => { useUi.setState({ accountEntry: "create" }); openSettings("account"); }}>{t("identity.createAccount")}</Button>
            <Button size="sm" onClick={() => setConfirmingBackup(true)}>{t("identity.exportBackup")}</Button>
            <button type="button" aria-label={t("common.close")} onClick={() => useSettings.getState().setOnboarding({ nudgeDismissed: true })} className="flex size-8 cursor-pointer items-center justify-center rounded text-muted hover:bg-hover hover:text-fg"><X className="size-4" /></button>
          </div>
        </div>
      )}
      {renewing && account && <PasswordDialog title={t("account.signInAgain")} body={t("account.renewBody")} confirmLabel={t("account.signInAgain")}
        onClose={() => setRenewing(false)} onSubmit={async (password) => { await accountActions.renew(password); }} />}
      {confirmingBackup && <Dialog open onOpenChange={(open) => { if (!open && !exporting) setConfirmingBackup(false); }}
        title={t("identity.exportBackup")} description={t("identity.backupSecret")} width="max-w-sm"
        footer={<>
          <Button disabled={exporting} onClick={() => setConfirmingBackup(false)}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={exporting} onClick={() => void backup()}>{t("common.save")}</Button>
        </>}>{null}</Dialog>}
    </>
  );
}
