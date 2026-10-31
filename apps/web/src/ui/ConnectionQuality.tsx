/**
 * Connection quality (docs/screen-share.md, "Connection quality"): a coloured signal indicator,
 * one brief message when upload congestion starts, and a details dialog behind the indicator.
 */
import { Signal, SignalHigh, SignalLow, SignalMedium, TriangleAlert, X } from "lucide-react";
import { useEffect, useState } from "react";
import { useLanguage, useT, type Key } from "../i18n";
import { cn } from "../lib/cn";
import type { Language } from "../state/settings";
import { closeConnectionDetails, dismissCongestionNotice, openConnectionDetails, useUplink, type ConnectionQuality, type Quality } from "../state/uplink";
import { qualityTone, useConnectionQuality, useVoiceConnectionActive } from "./hooks";
import { Dialog, Tooltip } from "./kit";

const ICONS = { good: SignalHigh, fair: SignalMedium, poor: SignalLow };
/** Long enough to read twice; hovering or focusing it holds it open. */
export const NOTICE_MS = 8_000;

/** "0,5" / "0.5": seconds, never less than 0.1, since a detected queue always adds some delay. */
const seconds = (lang: Language, ms: number) =>
  new Intl.NumberFormat(lang, { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(Math.max(0.1, ms / 1000));
const millis = (ms: number) => ms < 1 ? "<1" : String(Math.round(ms));

/** The dialog has no trigger of its own: focus goes back to whatever opened it, or to an indicator. */
let opener: HTMLElement | null = null;
function showDetails() {
  opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  openConnectionDetails();
}
function restoreFocus(event: Event) {
  const target = opener?.isConnected ? opener : document.querySelector<HTMLElement>('[data-testid="connection-quality"]');
  opener = null;
  if (!target) return;
  event.preventDefault();
  target.focus();
}

/** Lit bars over dimmed full ones, so one bar of four still reads as a signal meter. */
function Bars({ quality, className }: { quality: Quality; className: string }) {
  const Icon = ICONS[quality];
  return (
    <span aria-hidden className={cn("relative shrink-0", className)}>
      <Signal className="absolute inset-0 size-full opacity-30" />
      <Icon className="relative size-full" />
    </span>
  );
}

function qualityLabel(q: ConnectionQuality): Key {
  return q.congestion ? "voice.quality.upload" : `voice.quality.${q.quality}`;
}

/**
 * The always-on indicator. The panel shows it whenever there is a measurement (bars and ms, as the
 * round trip always was); the pill and the sharing bar only once it is not good.
 */
export function ConnectionIndicator({ variant, compact = false }: { variant: "panel" | "pill" | "bar"; compact?: boolean }) {
  const t = useT();
  const q = useConnectionQuality();
  const voiceConnected = useVoiceConnectionActive();
  if ((variant !== "bar" && !voiceConnected) || !q || (variant !== "panel" && q.quality === "good")) return null;
  const label = t(qualityLabel(q));
  const open = t("voice.quality.open", { quality: label, ms: millis(q.rttMs) });
  return (
    <Tooltip label={open}>
      <button
        type="button"
        data-testid="connection-quality"
        data-quality={q.quality}
        aria-label={open}
        aria-haspopup="dialog"
        // The sharing bar is a status region: the notice announces congestion once, not this chip.
        aria-live={variant === "bar" ? "off" : undefined}
        onClick={showDetails}
        className={cn(
          "t flex shrink-0 cursor-pointer items-center gap-0.5 tabular-nums focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent",
          variant === "panel" && "-my-0.5 ml-auto rounded px-1 py-0.5 hover:bg-hover",
          variant === "pill" && "size-8 justify-center rounded-md hover:bg-hover",
          variant === "bar" && "gap-1 rounded bg-raised px-1.5 text-xs leading-5 font-medium hover:brightness-110",
          qualityTone[q.quality],
        )}
      >
        <Bars quality={q.quality} className={variant === "pill" ? "size-[18px]" : "size-3.5"} />
        {variant === "panel" && `${millis(q.rttMs)} ms`}
        {variant === "bar" && !compact && <span>{label}</span>}
      </button>
    </Tooltip>
  );
}

/**
 * The brief message, once per congestion episode (state/uplink.ts). It lives in the toast stack's
 * polite live region, so a screen reader hears it once; its delay is fixed when it appears.
 */
export function CongestionNotice() {
  const t = useT();
  const lang = useLanguage();
  const notice = useUplink(s => s.notice);
  const voiceConnected = useVoiceConnectionActive();
  // Held per episode: a notice closed while hovered or touched never fires its pointerleave, and
  // the next episode must still collapse on its own.
  const [heldEpisode, setHeldEpisode] = useState<number | null>(null);
  const held = !!notice && heldEpisode === notice.episode;
  useEffect(() => {
    if (!notice || held) return;
    const timer = setTimeout(dismissCongestionNotice, NOTICE_MS);
    return () => clearTimeout(timer);
  }, [notice, held]);
  if (!voiceConnected || !notice) return null;
  return (
    <div
      role="status"
      data-testid="congestion-notice"
      onPointerEnter={() => setHeldEpisode(notice.episode)}
      onPointerLeave={() => setHeldEpisode(null)}
      onFocus={() => setHeldEpisode(notice.episode)}
      onBlur={() => setHeldEpisode(null)}
      className="anim-toast pointer-events-auto flex w-full max-w-sm items-start gap-2.5 rounded-lg border border-line-strong bg-raised py-2.5 pr-2 pl-3 text-sm shadow-pop"
    >
      <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warn" />
      <span className="min-w-0 flex-1 break-words">
        {t("voice.quality.notice", { delay: seconds(lang, notice.delayMs) })}{" "}
        <button type="button" onClick={showDetails} className="cursor-pointer font-medium text-accent underline underline-offset-2">
          {t("voice.quality.showDetails")}
        </button>
      </span>
      <button
        type="button"
        aria-label={t("common.close")}
        onClick={dismissCongestionNotice}
        className="t flex size-5 shrink-0 cursor-pointer items-center justify-center rounded text-subtle hover:bg-hover hover:text-fg"
      >
        <X className="size-3.5" />
      </button>
    </div>
  );
}

/** Mounted once: what the indicator means, what to do, and the numbers under "Technical details". */
export function ConnectionDetails() {
  const t = useT();
  const lang = useLanguage();
  const open = useUplink(s => s.details);
  const q = useConnectionQuality();
  const voiceConnected = useVoiceConnectionActive();
  const congestion = q?.congestion;
  const [explain, fix] = !q ? [t("voice.quality.none"), null]
    : congestion ? [t(voiceConnected ? "voice.quality.uploadExplain" : "screen.quality.uploadExplain", { delay: seconds(lang, congestion.rttMs - congestion.baselineMs) }), t("voice.quality.uploadFix")]
      : q.quality === "good" ? [t(voiceConnected ? "voice.quality.goodExplain" : "screen.quality.goodExplain"), null]
        : [t(voiceConnected ? "voice.quality.slowExplain" : "screen.quality.slowExplain"), t("voice.quality.slowFix")];
  const number = (n: number, digits = 0) => new Intl.NumberFormat(lang, { maximumFractionDigits: digits }).format(n);
  return (
    <Dialog open={open} onOpenChange={o => { if (!o) closeConnectionDetails(); }} title={t("voice.quality.title")} width="max-w-sm" onCloseAutoFocus={restoreFocus}>
      <div data-testid="connection-details" className="flex flex-col gap-3 text-sm leading-relaxed">
        {q && (
          <div className={cn("flex items-center gap-2 font-semibold", qualityTone[q.quality])}>
            <Bars quality={q.quality} className="size-5" />
            {t(qualityLabel(q))}
          </div>
        )}
        <p>{explain}</p>
        {fix && <p className="rounded-md bg-hover px-3 py-2">{fix}</p>}
        {q && (
          <details className="rounded-md border border-line px-3 py-1.5">
            <summary className="cursor-pointer py-0.5 text-[13px] font-medium text-muted focus-visible:outline-2 focus-visible:outline-accent">
              {t("voice.quality.technical")}
            </summary>
            <dl className="grid grid-cols-[1fr_auto] gap-x-4 gap-y-1 pt-2 pb-1 text-xs tabular-nums">
              <dt className="text-muted">{t("voice.quality.ping")}</dt>
              <dd className="text-right">{millis(q.rttMs)} ms</dd>
              {congestion && <>
                <dt className="text-muted">{t("voice.quality.usual")}</dt>
                <dd className="text-right">{millis(congestion.baselineMs)} ms</dd>
              </>}
              {q.lossPct !== null && <>
                <dt className="text-muted">{t("voice.quality.loss")}</dt>
                <dd className="text-right">{number(q.lossPct, 1)} %</dd>
              </>}
              {q.availableOutgoingBitrate !== null && <>
                <dt className="text-muted">{t("voice.quality.uploadRate")}</dt>
                <dd className="text-right">{number(q.availableOutgoingBitrate / 1e6, 1)} Mb/s</dd>
              </>}
            </dl>
            {congestion && <p className="pb-1.5 text-xs text-muted">{t(voiceConnected ? "voice.quality.uploadTech" : "screen.quality.uploadTech")}</p>}
          </details>
        )}
      </div>
    </Dialog>
  );
}
