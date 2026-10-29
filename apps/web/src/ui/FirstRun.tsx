import { useState, type FormEvent } from "react";
import { cn } from "../lib/cn";
import { useT } from "../i18n";
import { describeAccountError } from "../connect/errors";
import { validNickname } from "../net/nickname";
import { accountActions } from "../state/account";
import { useSettings } from "../state/settings";
import { Wordmark } from "./Brand";
import { Button, Field, Input, panelCard } from "./kit";

export function FirstRun({ card }: { card?: boolean }) {
  const t = useT();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const complete = async (skip = false) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (!skip) await accountActions.changeNickname(name);
      useSettings.getState().setOnboarding({ completed: true });
    } catch (e) {
      setError(describeAccountError(e, "session"));
    } finally { setBusy(false); }
  };
  const submit = (event: FormEvent) => { event.preventDefault(); void complete(); };
  return (
    <main className={cn("flex min-w-0 flex-1 items-center justify-center overflow-y-auto bg-surface px-4 py-8", card && panelCard)}>
      <form onSubmit={submit} className="flex w-full max-w-sm flex-col gap-4 rounded-2xl border border-line bg-side p-6">
        <Wordmark className="mb-2 h-10 w-auto self-center text-fg" />
        <h1 className="text-xl font-semibold tracking-tight">{t("onboarding.title")}</h1>
        <p className="text-sm text-muted">{t("onboarding.hint")}</p>
        <Field label={t("onboarding.name")}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} autoFocus autoComplete="nickname" maxLength={32} required />}
        </Field>
        {error && <p role="alert" className="text-sm text-danger">{error}</p>}
        <Button type="submit" variant="primary" busy={busy} disabled={!validNickname(name)}>{t("onboarding.continue")}</Button>
        <button type="button" disabled={busy} onClick={() => void complete(true)} className="cursor-pointer self-center rounded px-2 py-1 text-sm text-muted hover:text-fg">{t("onboarding.skip")}</button>
      </form>
    </main>
  );
}
