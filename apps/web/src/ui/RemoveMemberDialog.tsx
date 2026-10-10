import { useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { controller, describeRequestError } from "../state/controller";
import { useUi, type PersonRef, type ServerSettingsTab } from "../state/stores";
import { Button, Dialog } from "./kit";

/** Removes a member from the server. Not a ban: they can join again as a new member. */
export function RemoveMemberDialog({ person, back }: { person: PersonRef; back?: ServerSettingsTab }) {
  const t = useT();
  const [deleteMessages, setDeleteMessages] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const online = person.session !== undefined;

  const close = () => (back ? useUi.getState().openDialog({ kind: "serverSettings", tab: back }) : useUi.getState().closeDialog());

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await controller.removeMember(person.uid, deleteMessages);
      useUi.getState().toast("success", t("mod.removed", { name: person.nickname }));
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
      title={t("mod.removeTitle", { name: person.nickname })}
      description={t("mod.removeBody")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="danger" busy={busy} onClick={() => void submit()}>
            {t("mod.remove")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
        <label className="flex cursor-pointer items-start gap-2 text-sm text-muted">
          <input
            type="checkbox"
            checked={deleteMessages}
            onChange={(e) => setDeleteMessages(e.target.checked)}
            className="mt-0.5 size-4 accent-[var(--accent)]"
          />
          <span>
            {t("mod.removeMessages")}
            <span className="block text-xs text-subtle">{t("mod.removeMessagesHint")}</span>
          </span>
        </label>
        <p className="rounded-md bg-hover px-3 py-2 text-xs text-muted">{online ? t("mod.removeNoteOnline") : t("mod.removeNote")}</p>
        {error && (
          <p role="alert" className="rounded-md bg-danger-soft px-3 py-2 text-sm text-danger">
            {error}
          </p>
        )}
        <button type="submit" className="hidden" />
      </form>
    </Dialog>
  );
}
