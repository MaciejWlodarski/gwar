import { AlertCircle, CheckCircle2, Info, X } from "lucide-react";
import { useEffect, useRef } from "react";
import { useT } from "../i18n";
import { toastTimer } from "../lib/toast-timer";
import { useUi, type Toast } from "../state/stores";

function ToastItem({ toast: x }: { toast: Toast }) {
  const t = useT();
  const element = useRef<HTMLDivElement>(null);
  const dismiss = useUi((s) => s.dismissToast);
  useEffect(() => {
    if (x.sticky || !element.current) return;
    const node = element.current;
    const timer = toastTimer(x.kind === "error" ? 6000 : 3500, () => dismiss(x.id));
    let hovered = false;
    let disposed = false;
    const update = () => {
      if (disposed) return;
      if (document.hidden || hovered || node.contains(document.activeElement)) timer.pause();
      else timer.resume();
    };
    const enter = () => { hovered = true; update(); };
    const leave = () => { hovered = false; update(); };
    // focusout fires before activeElement changes.
    const blur = () => queueMicrotask(update);
    node.addEventListener("mouseenter", enter);
    node.addEventListener("mouseleave", leave);
    node.addEventListener("focusin", update);
    node.addEventListener("focusout", blur);
    document.addEventListener("visibilitychange", update);
    update();
    return () => {
      disposed = true;
      timer.pause();
      node.removeEventListener("mouseenter", enter);
      node.removeEventListener("mouseleave", leave);
      node.removeEventListener("focusin", update);
      node.removeEventListener("focusout", blur);
      document.removeEventListener("visibilitychange", update);
    };
  }, [x.id, x.kind, x.sticky, dismiss]);
  return (
    <div ref={element} role={x.kind === "error" ? "alert" : "status"} data-testid="toast"
      className="anim-toast pointer-events-auto flex w-full max-w-sm items-start gap-2.5 rounded-lg border border-line-strong bg-raised py-2.5 pr-2 pl-3 text-sm shadow-pop">
      {x.kind === "error" ? <AlertCircle className="mt-0.5 size-4 shrink-0 text-danger" />
        : x.kind === "success" ? <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-ok" />
        : <Info className="mt-0.5 size-4 shrink-0 text-accent" />}
      <div className="min-w-0 flex-1 break-words">
        <p>{x.text}</p>
        {x.details && <details className="mt-1 text-xs text-muted"><summary className="cursor-pointer">{t("common.details")}</summary><p className="mt-1">{x.details}</p></details>}
        {x.action && <button type="button" onClick={() => x.action!.run()} className="mt-2 cursor-pointer rounded border border-line-strong px-2.5 py-1 font-medium text-fg hover:bg-hover">{x.action.label}</button>}
      </div>
      <button aria-label={t("common.close")} onClick={() => dismiss(x.id)}
        className="t flex size-6 shrink-0 cursor-pointer items-center justify-center rounded text-muted hover:bg-hover hover:text-fg">
        <X className="size-3.5" />
      </button>
    </div>
  );
}

export function Toaster() {
  const toasts = useUi((s) => s.toasts);
  return (
    <div aria-live="polite" className="pointer-events-none fixed inset-x-0 bottom-20 z-[200] flex max-h-[70dvh] flex-col items-center gap-2 overflow-y-auto px-4 md:inset-x-auto md:right-4 md:bottom-4 md:w-[416px] md:items-end">
      {toasts.map((x) => <ToastItem key={x.id} toast={x} />)}
    </div>
  );
}
