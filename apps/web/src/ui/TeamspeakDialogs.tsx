import { useEffect, useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { controller, describeRequestError } from "../state/controller";
import { useSession, useUi, type PersonRef } from "../state/stores";
import { Button, Dialog, Field, Input } from "./kit";
import { useMemberName } from "./hooks";

/** TeamSpeak caps a poke's message at 100 characters. */
const POKE_MAX = 100;

/** Pokes someone on a TeamSpeak server: a message that pops up for them at once. */
export function PokeDialog({ person }: { person: PersonRef }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const name = useMemberName(person.uid, person.nickname);
  const [message, setMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (busy || person.session === undefined) return;
    setBusy(true);
    setError(null);
    try {
      await controller.pokeClient(person.session, message.trim());
      useUi.getState().toast("success", t("poke.sent", { name }));
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
      title={t("poke.dialogTitle", { name })}
      description={t("poke.dialogBody")}
      width="max-w-sm"
      footer={
        <>
          <Button onClick={close}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} onClick={() => void submit()}>
            {t("poke.send")}
          </Button>
        </>
      }
    >
      <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3">
        <Field label={t("poke.message")}>
          {(id) => <Input id={id} value={message} onChange={(e) => setMessage(e.target.value)} maxLength={POKE_MAX} autoFocus />}
        </Field>
        {error && <p role="alert" className="text-sm text-danger">{error}</p>}
      </form>
    </Dialog>
  );
}

/** A TeamSpeak channel's topic, description and limits. The description is plain text, never markup. */
export function ChannelInfoDialog({ channel: id }: { channel: number }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const channel = useSession((s) => s.channels[id]);
  const [description, setDescription] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    controller.channelDescription(id).then(
      (text) => alive && setDescription(text),
      (err) => alive && setError(describeRequestError(err)),
    );
    return () => {
      alive = false;
    };
  }, [id]);

  if (!channel) return null;
  const row = (label: string, value: string) => (
    <div className="flex justify-between gap-4 text-sm">
      <dt className="text-muted">{label}</dt>
      <dd className="text-fg">{value}</dd>
    </div>
  );
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && close()}
      title={channel.name}
      width="max-w-md"
      footer={<Button onClick={close}>{t("common.close")}</Button>}
    >
      <div className="flex flex-col gap-4">
        {channel.topic && (
          <section>
            <h3 className="text-xs font-medium uppercase text-subtle">{t("chinfo.topic")}</h3>
            <p className="wrap-anywhere text-sm">{channel.topic}</p>
          </section>
        )}
        <section>
          <h3 className="text-xs font-medium uppercase text-subtle">{t("chinfo.description")}</h3>
          {error ? (
            <p role="alert" className="text-sm text-danger">{error}</p>
          ) : description === null ? (
            <p className="text-sm text-muted">{t("chinfo.loading")}</p>
          ) : (
            <p data-testid="channel-description" className="max-h-72 overflow-y-auto whitespace-pre-wrap wrap-anywhere text-sm">
              {description.trim() || t("chinfo.noDescription")}
            </p>
          )}
        </section>
        <dl className="flex flex-col gap-1">
          {row(t("chinfo.limit"), channel.max_clients !== null ? String(channel.max_clients) : t("chinfo.none"))}
          {row(t("chinfo.familyLimit"), channel.max_family_clients != null ? String(channel.max_family_clients) : t("chinfo.none"))}
          {row(t("chinfo.password"), channel.has_password ? t("chinfo.passwordRequired") : t("chinfo.none"))}
        </dl>
      </div>
    </Dialog>
  );
}
