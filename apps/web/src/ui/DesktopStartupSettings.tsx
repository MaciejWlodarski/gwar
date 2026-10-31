import { useCallback, useEffect, useState } from "react";
import { useT } from "../i18n";
import { tauri } from "../platform";
import { useSettings } from "../state/settings";
import { Button, Switch } from "./kit";

interface StartupStatus { supported: boolean; openAtLogin: boolean; startMinimized: boolean; loginStatus: "disabled" | "enabled" | "approvalRequired" | "pending" | "legacy"; loginControl: "full" | "removeOnly"; registrationPresent: boolean }

/** Only successful native read-back changes the switches. Mounting never enables autostart. */
export function DesktopStartupSettings() {
  const t = useT();
  const desktop = useSettings(s => s.desktop);
  const [supported, setSupported] = useState(false);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [loginStatus, setLoginStatus] = useState<StartupStatus["loginStatus"]>("disabled");
  const [loginControl, setLoginControl] = useState<StartupStatus["loginControl"]>("full");
  const [registrationPresent, setRegistrationPresent] = useState(false);
  const [legacySeen, setLegacySeen] = useState(false);
  const apply = useCallback((status: StartupStatus) => {
    setSupported(status.supported);
    setLoginStatus(status.loginStatus);
    setLoginControl(status.loginControl ?? "full");
    setRegistrationPresent(status.registrationPresent ?? false);
    if (status.loginControl === "removeOnly" && status.registrationPresent) setLegacySeen(true);
    if (status.supported) useSettings.getState().setDesktop({
      // An old macOS can confirm entry removal, not the saved choice's effective authorization.
      ...(status.loginControl === "removeOnly" && status.registrationPresent ? {} : { openAtLogin: status.openAtLogin }),
      startMinimized: status.startMinimized,
    });
  }, []);
  const refresh = () => { setBusy(true); setAttempt(a => a + 1); };
  useEffect(() => {
    // Re-read after the person returns from System Settings; never poll or request consent.
    const focus = () => { if (!busy) { setBusy(true); setAttempt(a => a + 1); } };
    window.addEventListener("focus", focus);
    return () => window.removeEventListener("focus", focus);
  }, [busy]);
  useEffect(() => {
    let alive = true;
    void tauri().then(api => api.invoke<StartupStatus>("desktop_startup")).then(status => {
      if (!alive) return;
      apply(status);
      setError(false);
    }).catch(() => { if (alive) setError(true); }).finally(() => { if (alive) setBusy(false); });
    return () => { alive = false; };
  }, [attempt, apply]);
  const change = async (patch: Partial<Pick<StartupStatus, "openAtLogin" | "startMinimized">>) => {
    if (busy) return;
    setBusy(true);
    setError(false);
    try {
      const status = await (await tauri()).invoke<StartupStatus>("desktop_set_startup", patch);
      apply(status);
    } catch { setError(true); }
    finally { setBusy(false); }
  };
  const openSettings = async () => {
    if (busy) return;
    setBusy(true);
    setError(false);
    try { await (await tauri()).invoke("desktop_open_login_settings"); }
    catch { setError(true); }
    finally { setBusy(false); }
  };
  const removalOnly = loginControl === "removeOnly";
  return <>
    {supported && <>
      {(!removalOnly || registrationPresent || legacySeen) && <Switch checked={removalOnly ? registrationPresent : desktop.openAtLogin} disabled={busy || (removalOnly && !registrationPresent)} onCheckedChange={openAtLogin => void change({ openAtLogin })} label={t(removalOnly ? "desktop.loginSaved" : "desktop.openAtLogin")} description={t(removalOnly ? (registrationPresent ? "desktop.loginRemoveHint" : "desktop.loginRemovedHint") : "desktop.openAtLoginHint")} />}
      {(loginStatus === "approvalRequired" || loginStatus === "pending") && <div role="status" data-testid="startup-status" className="flex items-center justify-between gap-2 text-xs text-muted">
        <span>{t(loginStatus === "approvalRequired" ? "desktop.loginApproval" : "desktop.loginPending")}</span>
        <Button size="sm" disabled={busy} onClick={loginStatus === "approvalRequired" ? () => void openSettings() : refresh}>{t(loginStatus === "approvalRequired" ? "desktop.loginSettings" : "desktop.loginCheck")}</Button>
      </div>}
      <Switch checked={desktop.startMinimized} disabled={busy} onCheckedChange={startMinimized => void change({ startMinimized })} label={t("desktop.startMinimized")} description={t("desktop.startMinimizedHint")} />
    </>}
    {error && <div role="alert" className="flex items-center justify-between gap-2 text-sm text-danger"><span>{t("desktop.startupFailed")}</span><Button disabled={busy} onClick={refresh}>{t("desktop.retry")}</Button></div>}
  </>;
}
