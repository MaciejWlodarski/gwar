import { Bug, ImagePlus, X } from "lucide-react";
import { useEffect, useRef, useState, type ClipboardEvent, type DragEvent } from "react";
import { useT } from "../i18n";
import { connectApi } from "../connect/api";
import { encodeBase64Url } from "../net/base64url";
import { loadConnectRecord } from "../net/identity";
import { fitDiagnostics, fitScreenshot } from "../lib/diagnostics";
import { useAccount } from "../state/account";
import { controller, describeRequestError } from "../state/controller";
import { useUi } from "../state/stores";
import { Button, Dialog, Field, IconButton, Switch, Textarea, Tooltip } from "./kit";

const MAX_SHOTS = 5;

interface Shot { id: number; blob: Blob; url: string }

/** "Alpha" and the way to report a bug, at the bottom of the server rail. */
export function AlphaReport() {
  const t = useT();
  const openDialog = useUi((s) => s.openDialog);
  return (
    <div className="flex flex-col items-center gap-1">
      <Tooltip label={t("alpha.hint")} side="right">
        <span tabIndex={0} className="rounded bg-warn/15 px-1.5 py-0.5 text-[10px] font-semibold tracking-wide text-warn uppercase">
          {t("alpha.badge")}
        </span>
      </Tooltip>
      <IconButton label={t("report.open")} side="right" onClick={() => openDialog({ kind: "bugReport" })}>
        <Bug className="size-5" />
      </IconButton>
    </div>
  );
}

/** Text, optional diagnostics and up to five screenshots, sent privately to Gwar Connect. */
export function BugReportDialog() {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const toast = useUi((s) => s.toast);
  const account = useAccount((s) => s.account);
  const [text, setText] = useState("");
  const [withDiagnostics, setWithDiagnostics] = useState(true);
  const [diagnostics, setDiagnostics] = useState<Record<string, unknown> | null>(null);
  const [showDiagnostics, setShowDiagnostics] = useState(false);
  const [shots, setShots] = useState<Shot[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const nextId = useRef(0);
  const file = useRef<HTMLInputElement>(null);

  useEffect(() => { void controller.reportDiagnostics().then((d) => setDiagnostics(fitDiagnostics(d))); }, []);
  // Preview URLs are released with the dialog.
  const shotsRef = useRef<Shot[]>([]);
  useEffect(() => { shotsRef.current = shots; }, [shots]);
  useEffect(() => () => { for (const s of shotsRef.current) URL.revokeObjectURL(s.url); }, []);

  const add = async (files: Iterable<Blob>) => {
    setError(null);
    let room = MAX_SHOTS - shots.length;
    for (const f of files) {
      if (!f.type.startsWith("image/")) continue;
      if (room <= 0) { setError(t("report.tooManyShots", { max: MAX_SHOTS })); return; }
      try {
        const blob = await fitScreenshot(f);
        const shot = { id: nextId.current++, blob, url: URL.createObjectURL(blob) };
        room--;
        setShots((list) => [...list, shot]);
      } catch { setError(t("report.badShot")); }
    }
  };
  const remove = (id: number) => setShots((list) => list.filter((s) => {
    if (s.id === id) URL.revokeObjectURL(s.url);
    return s.id !== id;
  }));
  const onPaste = (e: ClipboardEvent) => {
    const images = [...e.clipboardData.files].filter((f) => f.type.startsWith("image/"));
    if (images.length) { e.preventDefault(); void add(images); }
  };
  const onDrop = (e: DragEvent) => {
    e.preventDefault();
    void add(e.dataTransfer.files);
  };

  const send = async () => {
    if (!text.trim() || busy) return;
    setBusy(true); setError(null);
    try {
      const images = await Promise.all(shots.map(async (s) => ({ mime: s.blob.type, data: encodeBase64Url(new Uint8Array(await s.blob.arrayBuffer())) })));
      const record = await loadConnectRecord().catch(() => undefined);
      const id = await connectApi.report({ text: text.trim(), diagnostics: withDiagnostics && diagnostics ? diagnostics : { app: { omitted: true } }, images }, record?.token);
      toast("success", t("report.sent", { id }));
      close();
    } catch (err) {
      setError(describeRequestError(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={t("report.title")}
      description={t("report.description")}
      width="max-w-lg"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={!text.trim()} onClick={() => void send()}>{t("report.send")}</Button>
        </>
      }
    >
      <div className="flex flex-col gap-3" onPaste={onPaste} onDragOver={(e) => e.preventDefault()} onDrop={onDrop}>
        <Field label={t("report.what")}>
          {(id) => <Textarea id={id} value={text} onChange={(e) => setText(e.target.value)} rows={5} maxLength={10_000} placeholder={t("report.placeholder")} autoFocus />}
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
        <Switch checked={withDiagnostics} onCheckedChange={setWithDiagnostics} label={t("report.diagnostics")} description={t("report.diagnosticsHint")} />
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
      </div>
    </Dialog>
  );
}
