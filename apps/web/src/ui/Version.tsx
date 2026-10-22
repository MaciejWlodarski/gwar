import { ArrowDownToLine, CircleArrowUp, TriangleAlert } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { installUpdate, retryUpdate, useUpdate, type UpdateStatus } from "../platform/updater";
import { useScreen } from "../state/screen";
import { useUi, useVoice } from "../state/stores";
import { Spinner, Tooltip } from "./kit";

/** The version and the commit it was built from, e.g. "v0.1.2 · 31b65bd": what a bug report should name. */
const APP_VERSION = `v${__APP_VERSION__} · ${__APP_REVISION__}`;

/** The version at the bottom of the server rail; a click copies it. */
export function VersionLabel() {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(APP_VERSION);
      toast("success", t("version.copied", { version: APP_VERSION }));
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };
  return (
    <Tooltip label={t("version.copy", { version: APP_VERSION })} side="right">
      <button
        onClick={() => void copy()}
        aria-label={t("version.copy", { version: APP_VERSION })}
        className="t flex max-w-full cursor-pointer flex-col items-center rounded px-1 font-mono text-[10px] leading-tight text-subtle hover:text-muted"
      >
        <span>v{__APP_VERSION__}</span>
        <span className="max-w-full truncate">{__APP_REVISION__}</span>
      </button>
    </Tooltip>
  );
}

function updateLabel(t: ReturnType<typeof useT>, status: Exclude<UpdateStatus, { phase: "none" }>): string {
  const { version } = status;
  switch (status.phase) {
    case "downloading":
      return status.progress === null
        ? t("update.downloading", { version })
        : t("update.downloadingProgress", { version, percent: Math.round(status.progress * 100) });
    case "ready":
      return t("update.ready", { version });
    case "installing":
      return t("update.installing", { version });
    case "failed":
      return t("update.failed", { version, error: status.error });
  }
}

/** A ring around the download icon: the share downloaded, or a spinning arc while the size is unknown. */
function ProgressRing({ progress }: { progress: number | null }) {
  const r = 14;
  const length = 2 * Math.PI * r;
  return (
    <svg viewBox="0 0 32 32" aria-hidden className={cn("absolute inset-0 size-8 -rotate-90", progress === null && "animate-spin")}>
      <circle cx="16" cy="16" r={r} fill="none" strokeWidth="2" className="stroke-line-strong" />
      <circle
        cx="16"
        cy="16"
        r={r}
        fill="none"
        strokeWidth="2"
        strokeLinecap="round"
        strokeDasharray={length}
        strokeDashoffset={length * (1 - (progress ?? 0.25))}
        className="stroke-accent transition-[stroke-dashoffset] duration-300"
      />
    </svg>
  );
}

/**
 * The desktop app's update, in the server rail: nothing while the app is up to date, a progress
 * ring while a new version downloads, then a button that installs it and restarts the app.
 * Restarting ends a call or a screen share, so then it asks first.
 */
export function UpdateButton() {
  const t = useT();
  const status = useUpdate((s) => s.status);
  const openDialog = useUi((s) => s.openDialog);
  const inCall = useVoice((s) => s.state === "starting" || s.state === "connected");
  const sharing = useScreen((s) => s.own !== null || s.watches.length > 0);
  if (status.phase === "none") return null;
  const label = updateLabel(t, status);
  const onClick = () => {
    if (status.phase === "failed") return retryUpdate();
    if (status.phase !== "ready") return;
    if (!inCall && !sharing) return installUpdate();
    openDialog({
      kind: "confirm",
      title: t("update.confirmTitle"),
      body: t("update.confirmBody", { version: status.version }),
      confirmLabel: t("update.confirm"),
      onConfirm: installUpdate,
    });
  };
  const clickable = status.phase === "ready" || status.phase === "failed";
  return (
    <Tooltip label={<span className="block max-w-64">{label}</span>} side="right">
      <button
        onClick={onClick}
        aria-label={label}
        aria-disabled={!clickable}
        className={cn(
          "t relative inline-flex size-8 shrink-0 items-center justify-center rounded-full",
          clickable ? "cursor-pointer" : "cursor-default",
          status.phase === "ready" && "bg-ok/15 text-ok hover:bg-ok hover:text-white",
          status.phase === "failed" && "text-warn hover:bg-hover",
          (status.phase === "downloading" || status.phase === "installing") && "text-muted",
        )}
      >
        {status.phase === "downloading" && (
          <>
            <ProgressRing progress={status.progress} />
            <ArrowDownToLine className="size-4" />
          </>
        )}
        {status.phase === "ready" && <CircleArrowUp className="size-5" />}
        {status.phase === "installing" && <Spinner />}
        {status.phase === "failed" && <TriangleAlert className="size-5" />}
      </button>
    </Tooltip>
  );
}
