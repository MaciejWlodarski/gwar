import { useMemo, useState } from "react";
import { useT } from "../i18n";
import { controller, describeRequestError } from "../state/controller";
import { useSession } from "../state/stores";
import { useOwnerView } from "./hooks";
import { Button, Field, Input, Select } from "./kit";

/**
 * The server's owner, the confirmers and the managers of private channels, as this device verified
 * them (docs/e2e-build.md, section 6, 6b). The owner names confirmers and can hand the server over;
 * the owner and confirmers maintain the managers.
 */
export function OwnershipTab() {
  const t = useT();
  const view = useOwnerView();
  const claimed = useSession((s) => s.owner);
  const reported = useSession((s) => s.managers);
  const members = useSession((s) => s.members);
  const me = useSession((s) => s.me?.uid ?? "");
  const needsPassword = controller.ownerSigningNeedsPassword();
  const [password, setPassword] = useState("");
  const [confirmers, setConfirmers] = useState<string[] | null>(null);
  const [heir, setHeir] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const name = (uid: string) => members[uid]?.nickname ?? `${uid.slice(0, 8)}…`;
  // Members, and confirmers no longer listed as members (removed people), so the owner can drop them.
  const people = useMemo(() => {
    const listed = Object.values(members).filter((m) => !m.uid.startsWith("ts:") && m.uid !== view?.owner);
    const gone = (view?.confirmers ?? []).filter((u) => !members[u]).map((uid) => ({ uid, nickname: `${uid.slice(0, 8)}…` }));
    return [...listed, ...gone].sort((a, b) => a.nickname.localeCompare(b.nickname));
  }, [members, view?.owner, view?.confirmers]);

  if (!claimed) return <p className="text-sm text-muted">{t("own.none")}</p>;
  if (!view) return <p className="text-sm text-muted">{t("own.verifying")}</p>;
  const isOwner = view.owner === me;
  const maintains = isOwner || view.confirmers.includes(me);
  const chosen = confirmers ?? view.confirmers;

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      setConfirmers(null);
      setPassword("");
    } catch (e) {
      setError(describeRequestError(e));
    }
    setBusy(false);
  };
  const passwordField = needsPassword && isOwner && (
    <Field label={t("own.password")} hint={t("own.passwordHint")}>
      {(id) => <Input id={id} type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" />}
    </Field>
  );
  const ready = !needsPassword || password.length > 0;

  return (
    <div className="flex flex-col gap-5">
      <section className="flex flex-col gap-1">
        <h3 className="text-sm font-medium">{t("own.owner")}</h3>
        <p className="text-sm">{name(view.owner)}</p>
        <p className="text-xs text-subtle">{t("own.verified")}</p>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-sm font-medium">{t("own.confirmers")}</h3>
        <p className="text-xs text-subtle">{t("own.confirmersHint")}</p>
        {isOwner ? (
          <div className="flex max-h-40 flex-col gap-1 overflow-y-auto">
            {people.map((m) => (
              <label key={m.uid} className="flex cursor-pointer items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={chosen.includes(m.uid)}
                  onChange={() => setConfirmers(chosen.includes(m.uid) ? chosen.filter((u) => u !== m.uid) : [...chosen, m.uid])}
                  className="size-4 accent-[var(--accent)]"
                />
                {m.nickname}
              </label>
            ))}
          </div>
        ) : (
          <p className="text-sm">{view.confirmers.map(name).join(", ") || t("own.nobody")}</p>
        )}
        {isOwner && confirmers !== null && (
          <>
            {passwordField}
            <Button disabled={busy || !ready} onClick={() => void run(() => controller.setConfirmers(chosen, password || undefined))}>
              {t("own.saveConfirmers")}
            </Button>
          </>
        )}
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-sm font-medium">{t("own.managers")}</h3>
        <p className="text-xs text-subtle">{t("own.managersHint")}</p>
        <ul className="flex flex-col gap-1">
          {view.managers.map((uid) => (
            <li key={uid} className="flex items-center justify-between text-sm">
              {name(uid)}
              {maintains && (
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => void run(() => controller.setManagers(view.managers.filter((m) => m !== uid)))}>
                  {t("own.remove")}
                </Button>
              )}
            </li>
          ))}
          {view.managers.length === 0 && <li className="text-sm text-muted">{t("own.nobody")}</li>}
        </ul>
        {(reported?.pending.length ?? 0) > 0 && (
          <>
            <p className="text-xs font-medium text-subtle">{t("own.pending")}</p>
            <ul className="flex flex-col gap-1">
              {reported!.pending.map((uid) => (
                <li key={uid} className="flex items-center justify-between text-sm">
                  {name(uid)}
                  {maintains && (
                    <Button
                      size="sm"
                      disabled={busy || (!reported?.anchor && !ready)}
                      onClick={() => void run(() => controller.setManagers([...view.managers, uid], password || undefined))}
                    >
                      {t("own.allow")}
                    </Button>
                  )}
                </li>
              ))}
            </ul>
            {!reported?.anchor && passwordField}
          </>
        )}
        {view.list_signer && (
          <p className="text-xs text-subtle">{t("own.signedBy", { name: name(view.list_signer), date: new Date(view.list_signed_at ?? 0).toLocaleDateString() })}</p>
        )}
      </section>

      {isOwner && (
        <section className="flex flex-col gap-2">
          <h3 className="text-sm font-medium">{t("own.transfer")}</h3>
          <p className="text-xs text-subtle">{t("own.transferHint")}</p>
          <Select value={heir} onValueChange={setHeir} placeholder={t("own.transferTo")} options={people.map((m) => ({ value: m.uid, label: m.nickname }))} />
          {heir && passwordField}
          {heir && (
            <Button variant="danger" disabled={busy || !ready} onClick={() => void run(() => controller.transferOwnership(heir, password || undefined))}>
              {t("own.transferConfirm", { name: name(heir) })}
            </Button>
          )}
        </section>
      )}
      {error && <p role="alert" className="text-sm text-danger">{error}</p>}
    </div>
  );
}
