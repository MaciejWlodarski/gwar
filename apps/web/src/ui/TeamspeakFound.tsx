import { useState } from "react";
import { useT } from "../i18n";
import { MIN_LEVEL, type TsFound } from "../connect/teamspeak";
import { shortUid } from "../connect/ts-list";
import { useUi } from "../state/stores";
import { Button, Dialog, Spinner } from "./kit";

/**
 * The identities of the official TeamSpeak client, with a checkbox each. The last used one is checked
 * to begin with; one below the minimum level can't be chosen, and says why. `found` null means
 * "still looking". Used for "Find on this computer" and for the first-run question.
 */
export function FoundDialog({
  mode,
  found,
  error,
  known,
  busy,
  onConfirm,
  onCancel,
}: {
  mode: "find" | "first-run";
  found: TsFound[] | null;
  error?: string | null;
  /** uids that are in the list already. */
  known?: ReadonlySet<string>;
  busy?: boolean;
  onConfirm: (chosen: TsFound[]) => void;
  /** Close (find) or "create a new one instead" (first run). */
  onCancel: () => void;
}) {
  const t = useT();
  // What the person ticked or cleared; anything untouched starts as "the client's last used one".
  const [touched, setTouched] = useState<ReadonlyMap<string, boolean>>(new Map());
  const usable = (f: TsFound) => f.level >= MIN_LEVEL && !known?.has(f.uid);
  const isChecked = (f: TsFound) => usable(f) && (touched.get(f.uid) ?? f.selected);
  const toggle = (f: TsFound) => setTouched((m) => new Map(m).set(f.uid, !isChecked(f)));
  const chosen = (found ?? []).filter(isChecked);
  const first = mode === "first-run";

  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onCancel()}
      title={t(first ? "ts.firstTitle" : "ts.findTitle")}
      description={t(first ? "ts.firstBody" : "ts.findBody")}
      footer={
        <>
          <Button onClick={onCancel}>{t(first ? "ts.firstNew" : "common.cancel")}</Button>
          <Button variant="primary" busy={busy} disabled={chosen.length === 0} onClick={() => onConfirm(chosen)}>
            {t(first ? "ts.firstUse" : "ts.findImport")}
          </Button>
        </>
      }
    >
      {found === null && !error && (
        <p className="flex items-center gap-2 text-sm text-muted">
          <Spinner /> {t("ts.findSearching")}
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-danger">
          {error}
        </p>
      )}
      {found !== null && found.length === 0 && <p className="text-sm text-muted">{t("ts.findNone")}</p>}
      {found !== null && found.length > 0 && (
        <ul aria-label={t("ts.findTitle")} className="flex flex-col gap-1">
          {found.map((f) => {
            const have = known?.has(f.uid);
            const reason = f.level < MIN_LEVEL ? t("ts.findLow", { level: f.level, min: MIN_LEVEL }) : have ? t("ts.findHave") : null;
            return (
              <li key={f.uid}>
                <label className={`t flex items-start gap-3 rounded-lg border border-line px-3 py-2 ${reason ? "opacity-60" : "cursor-pointer hover:bg-hover"}`}>
                  <input
                    type="checkbox"
                    className="mt-1 size-4 accent-accent"
                    checked={isChecked(f)}
                    disabled={!!reason}
                    onChange={() => toggle(f)}
                    aria-label={f.name}
                  />
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center gap-2">
                      <span className="truncate text-sm font-medium">{f.name}</span>
                      {f.selected && <span className="shrink-0 rounded bg-accent-soft px-1.5 py-px text-[10px] leading-4 font-semibold text-accent">{t("ts.findLastUsed")}</span>}
                    </span>
                    <span className="block truncate text-xs text-muted">
                      {f.source} · <span className="font-mono">{shortUid(f.uid)}</span> · {t("ts.level", { level: f.level })}
                    </span>
                    {reason && <span className="block text-xs text-warn">{reason}</span>}
                  </span>
                </label>
              </li>
            );
          })}
        </ul>
      )}
    </Dialog>
  );
}

/** The question before the first TeamSpeak connection: use the official client's identity, or make a new one. */
export function FirstRunDialog({ found, resolve }: { found: TsFound[]; resolve: (chosen: TsFound[] | null) => void }) {
  const closeDialog = useUi((s) => s.closeDialog);
  const answer = (chosen: TsFound[] | null) => {
    resolve(chosen);
    closeDialog();
  };
  return <FoundDialog mode="first-run" found={found} onConfirm={answer} onCancel={() => answer(null)} />;
}
