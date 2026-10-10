import { useState, type FormEvent } from "react";
import { useT, type Key } from "../i18n";
import { BAN_DURATIONS } from "../lib/ban";
import { controller, describeRequestError } from "../state/controller";
import { useUi, type PersonRef, type ServerSettingsTab } from "../state/stores";
import { useMemberName } from "./hooks";
import { Button, Dialog, Field, Input, Segmented } from "./kit";

export function BanDialog({ person, back }: { person: PersonRef; back?: ServerSettingsTab }) {
  const t = useT();
  const nickname = useMemberName(person.uid, person.nickname);
  const [reason, setReason] = useState("");
  const [duration, setDuration] = useState<(typeof BAN_DURATIONS)[number]["id"]>("1d");
  const [ip, setIp] = useState(false);
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
      await controller.createBan({
        ...(online ? { client: person.session } : { uid: person.uid }),
        ip: online && ip,
        ...(BAN_DURATIONS.find((d) => d.id === duration)?.seconds ? { duration: BAN_DURATIONS.find((d) => d.id === duration)!.seconds } : {}),
        ...(reason.trim() ? { reason: reason.trim() } : {}),
      });
      useUi.getState().toast("success", t("mod.banned", { name: nickname }));
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
      title={t("mod.banTitle", { name: nickname })}
      description={t("mod.banBody")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="danger" busy={busy} onClick={() => void submit()}>
            {t("mod.ban")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
        <Field label={t("mod.reason")} hint={t("mod.reasonHint")}>
          {(id) => <Input id={id} value={reason} onChange={(e) => setReason(e.target.value)} maxLength={200} autoFocus />}
        </Field>
        <Segmented
          label={t("mod.duration")}
          value={duration}
          onChange={setDuration}
          options={BAN_DURATIONS.map((d) => ({ value: d.id, label: t(`mod.dur.${d.id}` as Key) }))}
        />
        <label className="flex cursor-pointer items-start gap-2 text-sm text-muted">
          <input
            type="checkbox"
            checked={ip && online}
            disabled={!online}
            onChange={(e) => setIp(e.target.checked)}
            className="mt-0.5 size-4 accent-[var(--accent)]"
          />
          <span>
            {t("mod.alsoIp")}
            <span className="block text-xs text-subtle">{online ? t("mod.alsoIpHint") : t("mod.alsoIpOffline")}</span>
          </span>
        </label>
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
