import { useEffect, useState } from "react";
import { useT } from "../i18n";
import { tauri } from "../platform";
import { useSettings } from "../state/settings";
import { Button, Switch } from "./kit";

interface StartupStatus { supported: boolean; openAtLogin: boolean; startMinimized: boolean }

/** Only successful native read-back changes the switches. Mounting never enables autostart. */
export function DesktopStartupSettings() {
  const t = useT();
  const desktop = useSettings(s => s.desktop);
  const [supported, setSupported] = useState(false);
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let alive = true;
    void tauri().then(api => api.invoke<StartupStatus>("desktop_startup")).then(status => {
      if (!alive) return;
      setSupported(status.supported);
      useSettings.getState().setDesktop({ openAtLogin: status.openAtLogin, startMinimized: status.startMinimized });
      setError(false);
    }).catch(() => { if (alive) setError(true); }).finally(() => { if (alive) setBusy(false); });
    return () => { alive = false; };
  }, [attempt]);
  const change = async (patch: Partial<Pick<StartupStatus, "openAtLogin" | "startMinimized">>) => {
    if (busy) return;
    setBusy(true);
    setError(false);
    try {
      const status = await (await tauri()).invoke<StartupStatus>("desktop_set_startup", patch);
      useSettings.getState().setDesktop({ openAtLogin: status.openAtLogin, startMinimized: status.startMinimized });
    } catch { setError(true); }
    finally { setBusy(false); }
  };
  return <>
    {supported && <>
      <Switch checked={desktop.openAtLogin} disabled={busy} onCheckedChange={openAtLogin => void change({ openAtLogin })} label={t("desktop.openAtLogin")} description={t("desktop.openAtLoginHint")} />
      <Switch checked={desktop.startMinimized} disabled={busy} onCheckedChange={startMinimized => void change({ startMinimized })} label={t("desktop.startMinimized")} description={t("desktop.startMinimizedHint")} />
    </>}
    {error && <div role="alert" className="flex items-center justify-between gap-2 text-sm text-danger"><span>{t("desktop.startupFailed")}</span><Button disabled={busy} onClick={() => { setBusy(true); setAttempt(a => a + 1); }}>{t("desktop.retry")}</Button></div>}
  </>;
}
