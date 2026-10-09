import { AlertCircle, CheckCircle2, Info, X } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { useUi } from "../state/stores";

export function Toaster() {
  const t = useT();
  const toasts = useUi((s) => s.toasts);
  const dismiss = useUi((s) => s.dismissToast);
  return (
    <div
      aria-live="polite"
      className="pointer-events-none fixed inset-x-0 bottom-4 z-[200] flex flex-col items-center gap-2 px-4 md:inset-x-auto md:right-4 md:items-end"
    >
      {toasts.map((x) => (
        <div
          key={x.id}
          role={x.kind === "error" ? "alert" : "status"}
          className="anim-toast pointer-events-auto flex w-full max-w-sm items-start gap-2.5 rounded-lg border border-line-strong bg-raised py-2.5 pr-2 pl-3 text-sm shadow-pop"
        >
          {x.kind === "error" ? (
            <AlertCircle className="mt-0.5 size-4 shrink-0 text-danger" />
          ) : x.kind === "success" ? (
            <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-ok" />
          ) : (
            <Info className="mt-0.5 size-4 shrink-0 text-accent" />
          )}
          <span className={cn("min-w-0 flex-1 break-words")}>{x.text}</span>
          <button
            aria-label={t("common.close")}
            onClick={() => dismiss(x.id)}
            className="t flex size-5 shrink-0 cursor-pointer items-center justify-center rounded text-subtle hover:bg-hover hover:text-fg"
          >
            <X className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
