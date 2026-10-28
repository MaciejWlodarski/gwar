import { TriangleAlert } from "lucide-react";
import { useMemo } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { currentUploadCongestion, useUplink } from "../state/uplink";
import type { UplinkStatus } from "../voice/uplink-congestion";

/** Native disclosure works with a keyboard and touch, as well as the desktop title hint. */
export function UploadWarning({ status, sharing = false }: { status: UplinkStatus; sharing?: boolean }) {
  const t = useT();
  const hint = t("voice.uploadCongestedHint");
  return (
    // Live ms updates stay silent, also inside the sharing bar's status region; the announcer speaks once.
    <details data-testid="upload-warning" aria-live="off" className={cn("min-w-0 text-xs", sharing ? "rounded bg-raised px-1.5 text-warn" : "text-warn")}
      onKeyDown={e => { if (e.key === "Escape") e.currentTarget.open = false; }}>
      <summary title={hint} className="flex cursor-pointer list-none items-center gap-1 leading-5 font-medium tabular-nums focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
        <TriangleAlert aria-hidden className="size-3.5 shrink-0" />
        <span>{t("voice.uploadCongested", { ms: status.rttMs })}</span>
      </summary>
      <p className="max-w-sm py-2 leading-relaxed font-normal text-fg">{hint}</p>
    </details>
  );
}

/** Mounted once: a polite announcement when congestion starts, never for later RTT updates. */
export function UploadCongestionAnnouncer() {
  const t = useT();
  const congested = useUplink(s => !!(s.screen?.congested || s.voice?.congested));
  // The region exists before its text, so screen readers notice the insertion.
  const message = useMemo(() => {
    const status = congested ? currentUploadCongestion() : null;
    return status ? t("voice.uploadCongested", { ms: status.rttMs }) : "";
  }, [congested, t]);
  return <span role="status" aria-live="polite" className="sr-only" data-testid="upload-announcer">{message}</span>;
}
