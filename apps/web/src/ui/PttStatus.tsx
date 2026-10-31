import { Info } from "lucide-react";
import { useState } from "react";
import { useT } from "../i18n";
import { isDesktop, tauri } from "../platform";
import { useGlobalPtt } from "../platform/ptt";
import { Button, IconButton, Dialog } from "./kit";

/** One compact status row; permission and privacy details are available on demand. */
export function PttStatus() {
  const t = useT();
  const status = useGlobalPtt(s => s.status);
  const [details, setDetails] = useState(false);
  const [openFailed, setOpenFailed] = useState(false);
  const onDesktop = isDesktop();
  const permission = onDesktop && status === "permission";
  const key = !onDesktop ? "local" : status === "ready" ? "ready" : status === "legacy" ? "legacy" : status === "local_only" ? "local" : status === "unsupported_key" ? "unsupported" : permission ? "permission" : "recovering";
  return <>
    <div data-ptt-status className="flex min-w-0 items-center gap-2 text-xs">
      <p role="status" className="min-w-0 flex-1 text-subtle">{t(`audio.pttStatus.${key}`)}</p>
      {permission && <Button size="sm" onClick={() => {
        void tauri().then(api => api.invoke("open_ptt_settings")).catch(() => { setOpenFailed(true); setDetails(true); });
      }}>{t("audio.pttAllow")}</Button>}
      <IconButton label={t("audio.pttDetails")} onClick={() => setDetails(true)}><Info className="size-4" /></IconButton>
    </div>
    <Dialog open={details} onOpenChange={setDetails} title={t("audio.pttDetails")}>
      <div className="space-y-3 text-sm text-muted">
        <p>{t(`audio.pttDetail.${key}`)}</p>
        <p>{t("audio.pttPrivacy")}</p>
        {key !== "legacy" && key !== "local" && <p>{t("audio.pttRecovery")}</p>}
        {openFailed && <p role="alert">{t("audio.pttSettingsFailed")}</p>}
      </div>
    </Dialog>
  </>;
}
