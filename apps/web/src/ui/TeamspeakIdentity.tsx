import { Check, Copy, Download, FileUp, Pencil, Plus, Search, Star, Trash2, Upload } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { tNow, useT } from "../i18n";
import { ConnectApiError } from "../connect/api";
import { describeAccountError } from "../connect/errors";
import { tsBridge, type TsBridge, type TsFound, type TsListInfo } from "../connect/teamspeak";
import { deleteEntry, renameEntry, setDefault, shortUid, type TsEntry, type TsList } from "../connect/ts-list";
import { accountActions, useAccount } from "../state/account";
import { useUi } from "../state/stores";
import { Button, Dialog, Field, IconButton, Input, Textarea } from "./kit";
import { FoundDialog } from "./TeamspeakFound";

/** Tauri rejects with plain strings; the Connect client with its own error type. */
function errorText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof ConnectApiError) return describeAccountError(e, "session");
  return e instanceof Error ? e.message : String(e);
}

type VaultView = { locked: true } | { locked: false; list: TsList };

const Alert = ({ children }: { children: string }) => (
  <p role="alert" className="text-sm text-danger">
    {children}
  </p>
);

type Open =
  | { kind: "import" }
  | { kind: "generate" }
  | { kind: "find" }
  | { kind: "rename"; entry: TsEntry }
  | { kind: "export"; entry: TsEntry }
  | { kind: "delete"; entry: TsEntry };

/** The unique id, shortened; a click copies the whole of it. */
function CopyUid({ uid }: { uid: string }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(uid);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };
  return (
    <button
      type="button"
      title={t("ts.copyUid")}
      aria-label={`${t("ts.copyUid")}: ${uid}`}
      onClick={() => void copy()}
      className="t inline-flex cursor-pointer items-center gap-1 rounded font-mono hover:text-fg"
    >
      {shortUid(uid)}
      {copied ? <Check className="size-3" /> : <Copy className="size-3" />}
    </button>
  );
}

function Row({ entry, level, isDefault, actions }: { entry: TsEntry; level?: number; isDefault: boolean; actions?: ReactNode }) {
  const t = useT();
  return (
    <li className="flex items-center gap-2 rounded-lg border border-line px-3 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium">{entry.name}</span>
          {isDefault && (
            <span className="shrink-0 rounded bg-accent-soft px-1.5 py-px text-[10px] leading-4 font-semibold tracking-wide text-accent uppercase">
              {t("ts.default")}
            </span>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-x-2 text-xs text-muted">
          <CopyUid uid={entry.uid} />
          {level !== undefined && <span>{t("ts.level", { level })}</span>}
        </div>
      </div>
      {actions}
    </li>
  );
}

/**
 * Settings -> Account -> TeamSpeak identities. On the desktop it lists the identities TeamSpeak
 * servers can see you as and lets the person add, rename, export and delete them and choose the
 * default; in the browser (signed in) it only shows the list the desktop app uses.
 */
export function TeamspeakIdentity({ onUnlock }: { onUnlock?: () => void }) {
  const t = useT();
  const bridge = tsBridge();
  const account = useAccount((s) => s.account);
  const revision = useAccount((s) => s.tsRevision);
  const [info, setInfo] = useState<TsListInfo | null>(null);
  const [vault, setVault] = useState<VaultView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<Open | null>(null);
  const signedIn = !!account;
  const locked = signedIn && vault?.locked === true;

  useEffect(() => {
    let alive = true;
    const jobs: Array<Promise<unknown>> = [];
    if (bridge) jobs.push(bridge.list("active").then((i) => alive && setInfo(i)));
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
    const list = vault && !vault.locked ? vault.list : null;
    return (
      <>
        {list && list.identities.length > 0 && (
          <>
            <p className="text-sm text-muted">{t("ts.webLabel")}</p>
            <ul aria-label={t("ts.listLabel")} className="flex flex-col gap-2">
              {list.identities.map((e) => (
                <Row key={e.uid} entry={e} isDefault={e.uid === list.default && list.identities.length > 1} />
              ))}
            </ul>
          </>
        )}
        {list && list.identities.length === 0 && <p className="text-sm text-muted">{t("ts.webNone")}</p>}
        {lockedNote}
        {error && <Alert>{error}</Alert>}
      </>
    );
  }

  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(t("ts.changeFailed", { message: errorText(e) }));
    }
  };
  const change = (fn: (list: TsList) => TsList) => accountActions.changeTeamspeak(fn);
  const close = () => setOpen(null);
  const nextName = t("ts.unnamed", { n: (info?.identities.length ?? 0) + 1 });

  return (
    <>
      <p className="text-sm text-muted">{signedIn ? t("ts.introAccount") : t("ts.introDevice")}</p>
      {info && info.identities.length === 0 && <p className="text-sm text-muted">{t("ts.empty")}</p>}
      {info && info.identities.length > 0 && (
        <ul aria-label={t("ts.listLabel")} className="flex flex-col gap-2">
          {info.identities.map((e) => {
            const isDefault = e.uid === info.default;
            return (
              <Row
                key={e.uid}
                entry={e}
                level={e.level}
                isDefault={isDefault}
                actions={
                  <div className="flex shrink-0 items-center">
                    <IconButton label={t("ts.rename")} size="sm" disabled={locked} onClick={() => setOpen({ kind: "rename", entry: e })}>
                      <Pencil className="size-4" />
                    </IconButton>
                    <IconButton
                      label={t("ts.setDefault")}
                      size="sm"
                      disabled={locked || isDefault}
                      onClick={() => void act(() => change((l) => setDefault(l, e.uid)))}
                    >
                      <Star className="size-4" />
                    </IconButton>
                    <IconButton label={t("ts.export")} size="sm" onClick={() => setOpen({ kind: "export", entry: e })}>
                      <Download className="size-4" />
                    </IconButton>
                    <IconButton label={t("ts.delete")} size="sm" tone="danger" disabled={locked} onClick={() => setOpen({ kind: "delete", entry: e })}>
                      <Trash2 className="size-4" />
                    </IconButton>
                  </div>
                }
              />
            );
          })}
        </ul>
      )}
      {lockedNote}
      {error && <Alert>{error}</Alert>}
      <div className="flex flex-wrap items-center gap-2" role="group" aria-label={t("ts.add")}>
        <span className="flex items-center gap-1 text-sm text-muted">
          <Plus className="size-4" /> {t("ts.add")}
        </span>
        <Button size="sm" disabled={!info || locked} onClick={() => setOpen({ kind: "import" })}>
          <Upload className="size-4" /> {t("ts.import")}
        </Button>
        <Button size="sm" disabled={!info || locked} onClick={() => setOpen({ kind: "generate" })}>
          {t("ts.generate")}
        </Button>
        <Button size="sm" disabled={!info || locked} onClick={() => setOpen({ kind: "find" })}>
          <Search className="size-4" /> {t("ts.find")}
        </Button>
      </div>
      {open?.kind === "import" && <ImportDialog bridge={bridge} nextName={nextName} onClose={close} />}
      {open?.kind === "generate" && <GenerateDialog nextName={nextName} onClose={close} />}
      {open?.kind === "find" && <FindFlow bridge={bridge} known={new Set(info?.identities.map((e) => e.uid))} onClose={close} />}
      {open?.kind === "rename" && <RenameDialog entry={open.entry} onClose={close} />}
      {open?.kind === "export" && <ExportDialog entry={open.entry} onClose={close} />}
      {open?.kind === "delete" && <DeleteDialog entry={open.entry} signedIn={signedIn} onClose={close} />}
    </>
  );
}

function NameField({ value, onChange, placeholder, autoFocus }: { value: string; onChange: (v: string) => void; placeholder?: string; autoFocus?: boolean }) {
  const t = useT();
  return (
    <Field label={t("ts.name")}>
      {(id) => <Input id={id} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} maxLength={48} autoFocus={autoFocus} />}
    </Field>
  );
}

/** The `id=` line of an identity `.ini` is the name the TeamSpeak client gave it. */
const iniName = (text: string) => /^id\s*=\s*"?(.*?)"?\s*$/m.exec(text)?.[1] ?? "";

function ImportDialog({ bridge, nextName, onClose }: { bridge: TsBridge; nextName: string; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [text, setText] = useState("");
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const file = useRef<HTMLInputElement>(null);

  const pick = async (f: File | undefined) => {
    if (file.current) file.current.value = "";
    if (!f) return;
    const content = await f.text();
    setText(content);
    setName((current) => current || iniName(content));
  };

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const parsed = await bridge.parse(text);
      const added = await accountActions.addTeamspeak([{ uid: parsed.uid, identity: parsed.identity, name: name.trim() || nextName }]);
      toast(added > 0 ? "success" : "info", t(added > 0 ? "ts.added" : "ts.alreadyListed"));
      onClose();
    } catch (e) {
      setError(t("ts.importFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.importTitle")} description={t("ts.importBody")}>
      <form
        className="mt-2 flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void add();
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
        <NameField value={name} onChange={setName} placeholder={nextName} />
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button type="submit" variant="primary" busy={busy} disabled={!text.trim()}>
            {t("ts.add")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function GenerateDialog({ nextName, onClose }: { nextName: string; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const generate = async () => {
    setBusy(true);
    setError(null);
    try {
      await accountActions.generateTeamspeak(name.trim() || nextName);
      toast("success", t("ts.added"));
      onClose();
    } catch (e) {
      setError(t("ts.importFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.generateTitle")} description={t("ts.generateBody")}>
      <form
        className="mt-2 flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void generate();
        }}
      >
        <NameField value={name} onChange={setName} placeholder={nextName} autoFocus />
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button type="submit" variant="primary" busy={busy}>
            {t("ts.generateAction")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

/** "Find on this computer": looks only now, because the person asked. */
function FindFlow({ bridge, known, onClose }: { bridge: TsBridge; known: ReadonlySet<string>; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [found, setFound] = useState<TsFound[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let alive = true;
    bridge
      .detect()
      .then((f) => alive && setFound(f))
      .catch((e: unknown) => alive && setError(t("ts.findError", { message: errorText(e) })));
    return () => {
      alive = false;
    };
    // The search runs once per opening; a language change must not repeat it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bridge]);

  const add = async (chosen: TsFound[]) => {
    setBusy(true);
    try {
      const added = await accountActions.addTeamspeak(chosen.map((f) => ({ uid: f.uid, name: f.name, identity: f.identity })));
      toast(added > 0 ? "success" : "info", added > 0 ? t("ts.findAdded", { count: added }) : t("ts.alreadyListed"));
      onClose();
    } catch (e) {
      setError(t("ts.importFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  return <FoundDialog mode="find" found={found} error={error} known={known} busy={busy} onConfirm={(c) => void add(c)} onCancel={onClose} />;
}

function RenameDialog({ entry, onClose }: { entry: TsEntry; onClose: () => void }) {
  const t = useT();
  const [name, setName] = useState(entry.name);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const rename = async () => {
    setBusy(true);
    try {
      await accountActions.changeTeamspeak((l) => renameEntry(l, entry.uid, name));
      onClose();
    } catch (e) {
      setError(t("ts.changeFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.renameTitle")}>
      <form
        className="mt-2 flex flex-col gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          void rename();
        }}
      >
        <NameField value={name} onChange={setName} autoFocus />
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button type="submit" variant="primary" busy={busy} disabled={!name.trim()}>
            {t("common.save")}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function DeleteDialog({ entry, signedIn, onClose }: { entry: TsEntry; signedIn: boolean; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const remove = async () => {
    setBusy(true);
    try {
      await accountActions.changeTeamspeak((l) => deleteEntry(l, entry.uid));
      toast("success", t("ts.deleted"));
      onClose();
    } catch (e) {
      setError(t("ts.changeFailed", { message: errorText(e) }));
      setBusy(false);
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.deleteTitle", { name: entry.name })} description={t(signedIn ? "ts.deleteBodyAccount" : "ts.deleteBodyDevice")}>
      <div className="mt-2 flex flex-col gap-3">
        {error && <Alert>{error}</Alert>}
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>{t("common.cancel")}</Button>
          <Button variant="danger" busy={busy} autoFocus onClick={() => void remove()}>
            {t("ts.deleteAction")}
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

function ExportDialog({ entry, onClose }: { entry: TsEntry; onClose: () => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [copied, setCopied] = useState(false);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(entry.identity);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };

  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={t("ts.exportTitle", { name: entry.name })} description={t("ts.exportBody")}>
      <div className="mt-2 flex flex-col gap-3">
        <p className="text-sm text-warn">{t("ts.exportWarning")}</p>
        <Field label={t("ts.importField")}>
          {(id) => <Textarea id={id} readOnly rows={4} value={entry.identity} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
        </Field>
        <div className="flex justify-end gap-2">
          <Button onClick={() => void copy()}>
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
