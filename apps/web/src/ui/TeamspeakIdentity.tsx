import { Check, Copy, Download, FileUp, Upload } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { tNow, useT } from "../i18n";
import { ConnectApiError } from "../connect/api";
import { describeAccountError } from "../connect/errors";
import { tsBridge, type TsBridge, type TsIdentityInfo, type TsParsed } from "../connect/teamspeak";
import { accountActions, useAccount } from "../state/account";
import { useUi } from "../state/stores";
import { Button, Dialog, Field, Input, Textarea } from "./kit";

/** Tauri rejects with plain strings; the Connect client with its own error type. */
function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof ConnectApiError) return describeAccountError(e, "session");
  return e instanceof Error ? e.message : String(e);
}

type VaultView = { locked: true } | { locked: false; identity: { identity: string; uid: string } | null };

const Alert = ({ children }: { children: string }) => (
  <p role="alert" className="text-sm text-danger">
    {children}
  </p>
);

/**
 * Settings -> Account -> TeamSpeak identity. On the desktop it shows the identity
 * TeamSpeak servers see and lets the person import or export it; in the browser
 * (signed in) it only shows the uid the desktop app uses.
 */
export function TeamspeakIdentity({ onUnlock }: { onUnlock?: () => void }) {
  const t = useT();
  const bridge = tsBridge();
  const account = useAccount((s) => s.account);
  const revision = useAccount((s) => s.tsRevision);
  const [info, setInfo] = useState<TsIdentityInfo | null>(null);
  const [vault, setVault] = useState<VaultView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<"import" | "export" | null>(null);
  const signedIn = !!account;
  const locked = signedIn && vault?.locked === true;

  useEffect(() => {
    let alive = true;
    const jobs: Array<Promise<unknown>> = [];
    if (bridge) jobs.push(bridge.info().then((i) => alive && setInfo(i)));
    if (signedIn) jobs.push(accountActions.teamspeakInVault().then((v) => alive && setVault(v)));
    Promise.all(jobs)
      .then(() => alive && setError(null))
      .catch((e: unknown) => alive && setError(tNow("ts.loadFailed", { message: errorText(e) })));
    return () => {
      alive = false;
    };
  }, [bridge, signedIn, account?.hasVaultKey, revision]);

  const lockedNote = locked && (
    <div className="flex flex-wrap items-center gap-2 text-sm text-muted">
      <span>{t("ts.locked")}</span>
      {onUnlock && (
        <Button size="sm" onClick={onUnlock}>
          {t("ts.unlock")}
        </Button>
      )}
    </div>
  );

  // The browser cannot use TeamSpeak; it shows what the desktop app will use.
  if (!bridge) {
    const found = vault && !vault.locked ? vault.identity : null;
    return (
      <>
        {found && (
          <Field label={t("ts.webLabel")}>
            {(id) => <Input id={id} readOnly value={found.uid} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
          </Field>
        )}
        {vault && !vault.locked && !found && <p className="text-sm text-muted">{t("ts.webNone")}</p>}
        {lockedNote}
        {error && <Alert>{error}</Alert>}
      </>
    );
  }

  return (
    <>
      <p className="text-sm text-muted">{signedIn ? t("ts.introAccount") : t("ts.introDevice")}</p>
      {info && (
        <Field label={t("ts.uid")} hint={`${t("ts.level", { level: info.level })} · ${t(info.source === "account" ? "ts.sourceAccount" : "ts.sourceDevice")}`}>
          {(id) => <Input id={id} readOnly value={info.uid} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
        </Field>
      )}
      {lockedNote}
      {error && <Alert>{error}</Alert>}
      <div className="flex flex-wrap gap-2">
        <Button disabled={!info || locked} onClick={() => setDialog("import")}>
          <Upload className="size-4" /> {t("ts.import")}
        </Button>
        <Button disabled={!info} onClick={() => setDialog("export")}>
          <Download className="size-4" /> {t("ts.export")}
        </Button>
      </div>
      {dialog === "import" && info && <ImportDialog bridge={bridge} current={info} signedIn={signedIn} onClose={() => setDialog(null)} />}
      {dialog === "export" && info && <ExportDialog bridge={bridge} source={info.source} onClose={() => setDialog(null)} />}
    </>
  );
}

function ImportDialog({ bridge, current, signedIn, onClose }: { bridge: TsBridge; current: TsIdentityInfo; signedIn: boolean; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [text, setText] = useState("");
  const [parsed, setParsed] = useState<TsParsed | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const file = useRef<HTMLInputElement>(null);

  const check = async (value: string) => {
    setBusy(true);
    setError(null);
    try {
      setParsed(await bridge.parse(value));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  const pick = async (f: File | undefined) => {
    if (file.current) file.current.value = "";
    if (!f) return;
    const content = await f.text();
    setText(content);
    await check(content);
  };

  const replace = async () => {
    if (!parsed) return;
    setBusy(true);
    setError(null);
    try {
      await accountActions.replaceTeamspeak({ identity: parsed.identity, uid: parsed.uid });
      toast("success", t("ts.imported"));
      onClose();
    } catch (e) {
      setError(t("ts.importFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  if (parsed) {
    return (
      <Dialog
        open
        onOpenChange={(o) => !o && onClose()}
        title={t("ts.confirmTitle")}
        description={t(signedIn ? "ts.confirmAccount" : "ts.confirmDevice", { uid: parsed.uid, level: parsed.level, current: current.uid })}
      >
        <div className="mt-2 flex flex-col gap-3">
          <Field label={t("ts.newUid")}>
            {(id) => <Input id={id} readOnly value={parsed.uid} className="font-mono text-xs" />}
          </Field>
          {error && <Alert>{error}</Alert>}
          <div className="flex justify-end gap-2">
            <Button onClick={() => setParsed(null)}>{t("account.back")}</Button>
            <Button variant="danger" busy={busy} onClick={() => void replace()}>
              {t("ts.replace")}
            </Button>
          </div>
        </div>
      </Dialog>
    );
  }
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.importTitle")} description={t("ts.importBody")}>
      <form
        className="mt-2 flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void check(text);
        }}
      >
        <Field label={t("ts.importField")}>
          {(id) => (
            <Textarea
              id={id}
              rows={4}
              value={text}
              onChange={(e) => setText(e.target.value)}
              className="font-mono text-xs"
              spellCheck={false}
              autoCapitalize="none"
              autoCorrect="off"
              autoFocus
            />
          )}
        </Field>
        <div>
          <Button type="button" size="sm" onClick={() => file.current?.click()}>
            <FileUp className="size-4" /> {t("ts.importFile")}
          </Button>
          <input ref={file} type="file" accept=".ini,text/plain" className="hidden" onChange={(e) => void pick(e.target.files?.[0])} />
        </div>
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button type="submit" variant="primary" busy={busy} disabled={!text.trim()}>
            {t("ts.importContinue")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function ExportDialog({ bridge, source, onClose }: { bridge: TsBridge; source: "device" | "account"; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [exported, setExported] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let alive = true;
    bridge
      .export(source)
      .then((x) => alive && setExported(x.identity))
      .catch((e: unknown) => alive && setError(errorText(e)));
    return () => {
      alive = false;
    };
  }, [bridge, source]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(exported ?? "");
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.exportTitle")} description={t("ts.exportBody")}>
      <div className="mt-2 flex flex-col gap-3">
        <p className="text-sm text-warn">{t("ts.exportWarning")}</p>
        {exported && (
          <Field label={t("ts.importField")}>
            {(id) => <Textarea id={id} readOnly rows={4} value={exported} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
          </Field>
        )}
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button disabled={!exported} onClick={() => void copy()}>
            {copied ? <Check className="size-4" /> : <Copy className="size-4" />} {t("common.copy")}
          </Button>
          <Button variant="primary" onClick={onClose}>
            {t("common.close")}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
