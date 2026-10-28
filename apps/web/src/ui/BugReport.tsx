import { Bug, ImagePlus, X } from "lucide-react";
import { useEffect, useRef, useState, type ClipboardEvent, type DragEvent } from "react";
import { useT } from "../i18n";
import { fitDiagnostics, fitScreenshot } from "../lib/diagnostics";
import { useAccount } from "../state/account";
import { controller } from "../state/controller";
import { cn } from "../lib/cn";
import { leavePage, useEscapeBack } from "./hooks";
import { Button, Field, panelCard, Switch, Textarea, Tooltip } from "./kit";
import { useVoicePill } from "./nav";
import { clearReportDraft, newShotId, sendReport, useReportDraft } from "./report-draft";
import { VoicePill } from "./VoicePanel";

const MAX_SHOTS = 5;

/** "Alpha", at the bottom of the global rail. */
export function AlphaBadge() {
  const t = useT();
  return (
    <Tooltip label={t("alpha.hint")} side="right">
      <span tabIndex={0} className="rounded bg-warn/15 px-1.5 py-0.5 text-[10px] font-semibold tracking-wide text-warn uppercase">
        {t("alpha.badge")}
      </span>
    </Tooltip>
  );
}

/** Text, optional diagnostics and up to five screenshots, sent privately to Gwar Connect. Esc goes back. */
export function BugReportSection({ card }: { card?: boolean }) {
  const t = useT();
  const pill = useVoicePill();
  const account = useAccount((s) => s.account);
  const { text, withDiagnostics, shots, busy, error } = useReportDraft();
  const draft = useReportDraft.setState;
  const [diagnostics, setDiagnostics] = useState<Record<string, unknown> | null>(null);
  const [showDiagnostics, setShowDiagnostics] = useState(false);
  const file = useRef<HTMLInputElement>(null);

  useEscapeBack();
  useEffect(() => { void controller.reportDiagnostics().then((d) => setDiagnostics(fitDiagnostics(d))); }, []);

  const add = async (files: Iterable<Blob>) => {
    draft({ error: null });
    let room = MAX_SHOTS - shots.length;
    for (const f of files) {
      if (!f.type.startsWith("image/")) continue;
      if (room <= 0) { draft({ error: t("report.tooManyShots", { max: MAX_SHOTS }) }); return; }
      try {
        const blob = await fitScreenshot(f);
        const shot = { id: newShotId(), blob, url: URL.createObjectURL(blob) };
        room--;
        draft((d) => ({ shots: [...d.shots, shot] }));
      } catch { draft({ error: t("report.badShot") }); }
    }
  };
  const remove = (id: number) => draft((d) => ({ shots: d.shots.filter((s) => {
    if (s.id === id) URL.revokeObjectURL(s.url);
    return s.id !== id;
  }) }));
  const onPaste = (e: ClipboardEvent) => {
    const images = [...e.clipboardData.files].filter((f) => f.type.startsWith("image/"));
    if (images.length) { e.preventDefault(); void add(images); }
  };
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    void add(e.dataTransfer.files);
  };

  const cancel = () => {
    clearReportDraft();
    leavePage();
  };

  return (
    <main aria-label={t("report.title")} className={cn("relative flex min-w-0 flex-1 flex-col bg-surface", card && panelCard)}>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-3 md:px-4">
        <Bug className="size-5 text-subtle" />
        <h1 className="truncate text-[15px] font-semibold">{t("report.title")}</h1>
      </header>
      <div className={cn("min-h-0 flex-1 overflow-y-auto", pill && "pb-20")}>
        <div className="mx-auto flex w-full max-w-2xl flex-col gap-3 p-4 md:p-8" onPaste={onPaste} onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
          <p className="text-sm text-muted">{t("report.description")}</p>
          <Field label={t("report.what")}>
            {(id) => <Textarea id={id} value={text} onChange={(e) => draft({ text: e.target.value })} rows={5} maxLength={10_000} placeholder={t("report.placeholder")} autoFocus data-entry />}
          </Field>
          <div className="flex flex-col gap-2">
            <div className="flex flex-wrap items-center gap-2">
              {shots.map((s) => (
                <span key={s.id} className="relative">
                  <img src={s.url} alt="" className="size-16 rounded-md border border-line object-cover" />
                  <button type="button" aria-label={t("report.removeShot")} onClick={() => remove(s.id)}
                    className="absolute -top-1.5 -right-1.5 flex size-5 cursor-pointer items-center justify-center rounded-full bg-surface text-muted shadow ring-1 ring-line hover:text-danger">
                    <X className="size-3" />
                  </button>
                </span>
              ))}
              {shots.length < MAX_SHOTS && (
                <Button size="sm" onClick={() => file.current?.click()}>
                  <ImagePlus className="size-4" /> {t("report.addShot")}
                </Button>
              )}
              <input ref={file} type="file" accept="image/png,image/jpeg,image/webp,image/gif" multiple hidden
                onChange={(e) => { void add(e.target.files ?? []); e.target.value = ""; }} />
            </div>
            <p className="text-xs text-subtle">{t("report.shotHint")}</p>
          </div>
          <Switch checked={withDiagnostics} onCheckedChange={(on) => draft({ withDiagnostics: on })} label={t("report.diagnostics")} description={t("report.diagnosticsHint")} />
          {withDiagnostics && (
            <div>
              <button type="button" className="cursor-pointer text-xs text-accent underline underline-offset-2" onClick={() => setShowDiagnostics((v) => !v)}>
                {t(showDiagnostics ? "report.hideDiagnostics" : "report.showDiagnostics")}
              </button>
              {showDiagnostics && (
                <pre className="mt-1 max-h-48 overflow-auto rounded-md bg-side p-2 text-[11px] whitespace-pre-wrap text-muted">
                  {diagnostics ? JSON.stringify(diagnostics, null, 2) : "…"}
                </pre>
              )}
            </div>
          )}
          <p className="text-xs text-subtle">{account ? t("report.privacyAccount", { handle: account.handle }) : t("report.privacy")}</p>
          {error && <p role="alert" className="text-sm text-danger">{error}</p>}
          <div className="flex justify-end gap-2 pt-1">
            <Button onClick={cancel}>{t("common.cancel")}</Button>
            <Button variant="primary" busy={busy} disabled={!text.trim()} onClick={() => void sendReport(diagnostics)}>{t("report.send")}</Button>
          </div>
        </div>
      </div>
      <VoicePill />
    </main>
  );
}
