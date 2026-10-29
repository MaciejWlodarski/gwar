import { ShieldAlert, X } from "lucide-react";
import { useEffect, useState } from "react";
import { useLanguage, useT, countKey } from "../i18n";
import { certificateWarning } from "../connect/expiry";
import { MEMBER_GROUP } from "../lib/permissions";
import { useIsMobile } from "../lib/media";
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
  const mobile = useIsMobile();
  const { account, certChecked, loaded } = useAccount();
  const now = useNoticeTime();
  const onboarding = useSettings((s) => s.onboarding);
  const hasRole = useSession((s) => s.kind === "gwar" && !!s.me && !!s.members[s.me.uid]?.groups.some((id) => id !== MEMBER_GROUP));
  const openSettings = useUi((s) => s.openSettings);
  const [renewing, setRenewing] = useState(false);
  const [identityDialog, setIdentityDialog] = useState<"choices" | "backup" | null>(null);
  const [exporting, setExporting] = useState(false);
  useEffect(() => {
    if (hasRole && onboarding && !onboarding.roleReceived) useSettings.getState().setOnboarding({ roleReceived: true });
  }, [hasRole, onboarding]);
  const warning = account ? certificateWarning(account.expiresAt, now, certChecked) : { level: "none" as const };
  const nudge = loaded && identityNudgeDue(onboarding, !!account, now, navigator.userAgent);
  const backup = async () => {
    setExporting(true);
    try { await exportIdentityBackup(); setIdentityDialog(null); }
    catch { useUi.getState().toast("error", t("identity.exportFailed")); }
    finally { setExporting(false); }
  };
  const createAccount = () => {
    setIdentityDialog(null);
    useUi.setState({ accountEntry: "create" });
    openSettings("account");
  };
  const dismissReminder = <button type="button" aria-label={t("common.close")} onClick={() => useSettings.getState().setOnboarding({ nudgeDismissed: true })} className="flex size-8 shrink-0 cursor-pointer items-center justify-center rounded text-muted hover:bg-hover hover:text-fg"><X className="size-4" /></button>;
  return (
    <>
      {warning.level !== "none" && (
        <div data-testid="expiry-banner" role="alert" className={mobile ? "flex shrink-0 items-center gap-2 border-b border-line bg-raised px-3 py-3 text-sm" : "flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-line bg-raised px-4 py-3 text-sm"}>
          <ShieldAlert className="size-4 shrink-0 text-warn" />
          <p className={mobile ? "min-w-0 flex-1 whitespace-nowrap" : "min-w-0 flex-1 basis-56"}>{warning.level === "expired" ? t(mobile ? "account.signInExpiredShort" : "account.signInExpired") : t(countKey(language, mobile ? "account.signInDaysShort" : "account.signInDays", warning.daysLeft), { count: warning.daysLeft })}</p>
          <Button size="sm" className={mobile ? "px-2!" : undefined} onClick={() => setRenewing(true)}>{t(mobile ? "account.signInShort" : "account.signInAgain")}</Button>
        </div>
      )}
      {nudge && (
        <div data-testid="identity-banner" role="status" className={mobile ? "flex shrink-0 items-center gap-1.5 border-b border-line bg-raised px-2 py-3 text-sm" : "flex shrink-0 flex-wrap items-center gap-x-3 gap-y-2 border-b border-line bg-raised px-4 py-3 text-sm"}>
          <ShieldAlert className="size-4 shrink-0 text-warn" />
          <p className={mobile ? "min-w-0 flex-1 text-[13px] whitespace-nowrap" : "min-w-0 flex-1 basis-56"}>{t(mobile ? "identity.protectTitle" : "identity.keepIdentity")}</p>
          {mobile ? <>
            <Button size="sm" className="px-2!" onClick={() => setIdentityDialog("choices")}>{t("identity.protect")}</Button>
            {dismissReminder}
          </> : <div className="flex shrink-0 items-center gap-2">
            <Button size="sm" onClick={createAccount}>{t("identity.createAccount")}</Button>
            <Button size="sm" onClick={() => setIdentityDialog("backup")}>{t("identity.exportBackup")}</Button>
            {dismissReminder}
          </div>}
        </div>
      )}
      {renewing && account && <PasswordDialog title={t("account.signInAgain")} body={t("account.renewBody")} confirmLabel={t("account.signInAgain")}
        onClose={() => setRenewing(false)} onSubmit={async (password) => { await accountActions.renew(password); }} />}
      {identityDialog && <Dialog open onOpenChange={(open) => { if (!open && !exporting) setIdentityDialog(null); }}
        title={t(identityDialog === "choices" ? "identity.protectTitle" : "identity.exportBackup")}
        description={t(identityDialog === "choices" ? "identity.lossWarning" : "identity.backupSecret")} width="max-w-sm"
        footer={identityDialog === "backup" && <>
          <Button disabled={exporting} onClick={() => setIdentityDialog(null)}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={exporting} onClick={() => void backup()}>{t("common.save")}</Button>
        </>}>{identityDialog === "choices" && <div className="mt-2 flex flex-col gap-2">
          <Button variant="primary" onClick={createAccount}>{t("identity.createConnectAccount")}</Button>
          <Button onClick={() => setIdentityDialog("backup")}>{t("identity.exportBackup")}</Button>
        </div>}</Dialog>}
    </>
  );
}
