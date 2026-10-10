import { AlertTriangle, Check, Copy, Download, KeyRound, LogOut, RefreshCw, ShieldCheck, Smartphone } from "lucide-react";
import { useCallback, useEffect, useState, type FormEvent, type ReactNode } from "react";
import { tNow, useT } from "../i18n";
import { normalizeHandle } from "../connect/account";
import type { DeviceInfo } from "../connect/api";
import { describeAccountError, type AccountContext } from "../connect/errors";
import { MIN_PASSWORD_LENGTH, passwordStrength } from "../connect/crypto";
import { certificateWarning } from "../connect/expiry";
import { cn } from "../lib/cn";
import { loadOrCreateIdentity, uidForPublicKey } from "../net/identity";
import { accountActions, useAccount } from "../state/account";
import { useSettings } from "../state/settings";
import { useUi } from "../state/stores";
import { Button, Dialog, Field, Input, Segmented } from "./kit";
import { TeamspeakIdentity } from "./TeamspeakIdentity";
import { tsBridge } from "../connect/teamspeak";

function Banner({ children }: { children: ReactNode }) {
  return (
    <div role="alert" className="flex gap-2 rounded-lg bg-danger-soft px-3 py-2 text-sm text-danger">
      <AlertTriangle className="mt-0.5 size-4 shrink-0" />
      <span>{children}</span>
    </div>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="mb-6 last:mb-0">
      <h3 className="mb-2 text-xs font-semibold tracking-wide text-subtle uppercase">{title}</h3>
      <div className="flex flex-col gap-3">{children}</div>
    </section>
  );
}

/** Runs an async action with a busy flag and an error message. */
function useAction(context: AccountContext) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const run = async (fn: () => Promise<void>): Promise<boolean> => {
    setBusy(true);
    setError(null);
    try {
      await fn();
      return true;
    } catch (e) {
      setError(describeAccountError(e, context));
      return false;
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, setError, run };
}

function NewPasswordFields({
  password,
  onPassword,
  confirm,
  onConfirm,
  label,
}: {
  password: string;
  onPassword: (v: string) => void;
  confirm: string;
  onConfirm: (v: string) => void;
  label?: string;
}) {
  const t = useT();
  const strength = password ? passwordStrength(password) : null;
  return (
    <>
      <Field
        label={label ?? t("account.password")}
        hint={
          strength && (
            <span className={cn(strength === "weak" ? "text-danger" : strength === "fair" ? "text-warn" : "text-ok")}>
              {t(`account.strength.${strength}`)}
            </span>
          )
        }
      >
        {(id) => (
          <Input id={id} type="password" value={password} onChange={(e) => onPassword(e.target.value)} autoComplete="new-password" required />
        )}
      </Field>
      <Field label={t("account.passwordConfirm")}>
        {(id) => <Input id={id} type="password" value={confirm} onChange={(e) => onConfirm(e.target.value)} autoComplete="new-password" required />}
      </Field>
    </>
  );
}

/** Returns an error text if the new password pair is not acceptable. */
function checkNewPassword(password: string, confirm: string): string | null {
  if ([...password].length < MIN_PASSWORD_LENGTH) return tNow("account.passwordShort", { min: MIN_PASSWORD_LENGTH });
  if (password !== confirm) return tNow("account.passwordMismatch");
  return null;
}

// ---------------------------------------------------------------- signed out

type Mode = "signin" | "create" | "recover";

export function AccountTab() {
  const t = useT();
  const loaded = useAccount((s) => s.loaded);
  const account = useAccount((s) => s.account);
  const pending = useAccount((s) => s.pending);
  const [mode, setMode] = useState<Mode>("signin");
  if (pending) return <RecoveryCode code={pending.recoveryCode} handle={pending.prepared.handle} />;
  if (account) return <AccountView />;
  if (!loaded) return null;
  return (
    <div>
      <p className="mb-4 text-sm text-muted">{t("account.intro")}</p>
      {mode === "signin" && <SignInForm onMode={setMode} />}
      {mode === "create" && <CreateForm onMode={setMode} />}
      {mode === "recover" && <RecoverForm onMode={setMode} />}
      {tsBridge() && (
        <div className="mt-8">
          <TeamspeakSection />
        </div>
      )}
    </div>
  );
}

/** The desktop app's TeamSpeak identity; in the browser only for a signed-in account. */
function TeamspeakSection({ onUnlock }: { onUnlock?: () => void }) {
  const t = useT();
  return (
    <Section title={t("ts.title")}>
      <TeamspeakIdentity onUnlock={onUnlock} />
    </Section>
  );
}

function LinkButton({ children, onClick }: { children: ReactNode; onClick: () => void }) {
  return (
    <button type="button" onClick={onClick} className="cursor-pointer self-start text-sm text-muted underline hover:text-fg">
      {children}
    </button>
  );
}

function HandleField({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const t = useT();
  return (
    <Field label={t("account.handle")} hint={t("account.handleHint")}>
      {(id) => (
        <Input
          id={id}
          value={value}
          onChange={(e) => onChange(e.target.value)}
          autoCapitalize="none"
          autoCorrect="off"
          spellCheck={false}
          autoComplete="username"
          maxLength={33}
          required
        />
      )}
    </Field>
  );
}

function SignInForm({ onMode }: { onMode: (m: Mode) => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [handle, setHandle] = useState("");
  const [password, setPassword] = useState("");
  const { busy, error, setError, run } = useAction("signin");
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const h = normalizeHandle(handle);
    if (!h) return setError(t("account.err.password"));
    if (await run(() => accountActions.signIn({ handle: h, password }))) {
      const a = useAccount.getState().account;
      toast("success", t("account.signedIn", { handle: a?.handle ?? h }));
    }
  };
  return (
    <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3" aria-label={t("account.signInTitle")}>
      <HandleField value={handle} onChange={setHandle} />
      <Field label={t("account.password")}>
        {(id) => <Input id={id} type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" required />}
      </Field>
      {error && <Banner>{error}</Banner>}
      <Button type="submit" variant="primary" busy={busy} className="self-start">
        {t("account.signIn")}
      </Button>
      <LinkButton onClick={() => onMode("recover")}>{t("account.forgot")}</LinkButton>
      <LinkButton onClick={() => onMode("create")}>{t("account.noAccount")}</LinkButton>
    </form>
  );
}

function CreateForm({ onMode }: { onMode: (m: Mode) => void }) {
  const t = useT();
  const [handle, setHandle] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [keep, setKeep] = useState<"keep" | "fresh">("keep");
  const { busy, error, setError, run } = useAction("register");
  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!normalizeHandle(handle)) return setError(t("account.handleInvalid"));
    const bad = checkNewPassword(password, confirm);
    if (bad) return setError(bad);
    void run(() => accountActions.create({ handle, password, keepIdentity: keep === "keep" }));
  };
  return (
    <form onSubmit={submit} className="flex flex-col gap-3" aria-label={t("account.createTitle")}>
      <HandleField value={handle} onChange={setHandle} />
      <NewPasswordFields password={password} onPassword={setPassword} confirm={confirm} onConfirm={setConfirm} />
      <div className="flex flex-col gap-1.5">
        <span className="text-xs font-medium text-muted">{t("account.identityChoice")}</span>
        <Segmented
          label={t("account.identityChoice")}
          value={keep}
          onChange={setKeep}
          options={[
            { value: "keep", label: t("account.keep") },
            { value: "fresh", label: t("account.fresh") },
          ]}
        />
        <p className="text-xs text-subtle">{keep === "keep" ? t("account.keepHint") : t("account.freshHint")}</p>
      </div>
      {error && <Banner>{error}</Banner>}
      <Button type="submit" variant="primary" busy={busy} className="self-start">
        {t("account.create")}
      </Button>
      <LinkButton onClick={() => onMode("signin")}>{t("account.haveAccount")}</LinkButton>
    </form>
  );
}

function RecoverForm({ onMode }: { onMode: (m: Mode) => void }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [handle, setHandle] = useState("");
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const { busy, error, setError, run } = useAction("recover");
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const bad = checkNewPassword(password, confirm);
    if (bad) return setError(bad);
    if (await run(() => accountActions.recover({ handle, code, newPassword: password }))) {
      toast("success", t("account.signedIn", { handle: useAccount.getState().account?.handle ?? handle }));
    }
  };
  return (
    <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3" aria-label={t("account.recoverTitle")}>
      <HandleField value={handle} onChange={setHandle} />
      <Field label={t("account.recoveryCode")} hint={t("account.recoveryCodeHint")}>
        {(id) => (
          <Input
            id={id}
            value={code}
            onChange={(e) => setCode(e.target.value)}
            className="font-mono text-xs"
            autoCapitalize="characters"
            autoCorrect="off"
            spellCheck={false}
            placeholder="XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX"
            required
          />
        )}
      </Field>
      <NewPasswordFields password={password} onPassword={setPassword} confirm={confirm} onConfirm={setConfirm} label={t("account.newPassword")} />
      {error && <Banner>{error}</Banner>}
      <Button type="submit" variant="primary" busy={busy} className="self-start">
        {t("account.recover")}
      </Button>
      <LinkButton onClick={() => onMode("signin")}>{t("account.haveAccount")}</LinkButton>
    </form>
  );
}

// ------------------------------------------------------------ recovery code

/**
 * The account is only prepared here: the service hears about it when the person confirms. If registering
 * fails (handle taken, rate limit) the code stays and can be tried again, under another handle if needed.
 */
function RecoveryCode({ code, handle: prepared }: { code: string; handle: string }) {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [saved, setSaved] = useState(false);
  const [copied, setCopied] = useState(false);
  const [failed, setFailed] = useState(false);
  const [handle, setHandle] = useState(prepared);
  const { busy, error, setError, run } = useAction("register");
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };
  const download = () => {
    const url = URL.createObjectURL(new Blob([`Gwar recovery code\n${code}\n`], { type: "text/plain" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = "gwar-recovery-code.txt";
    a.click();
    URL.revokeObjectURL(url);
  };
  return (
    <div className="flex flex-col gap-3">
      <h3 className="flex items-center gap-2 text-base font-semibold">
        <KeyRound className="size-4" /> {t("account.recoveryTitle")}
      </h3>
      <p className="text-sm text-muted">{t("account.recoveryBody")}</p>
      <div
        data-testid="recovery-code"
        className="rounded-lg border border-line-strong bg-side px-4 py-3 text-center font-mono text-sm tracking-wider select-all"
      >
        {code}
      </div>
      <div className="flex flex-wrap gap-2">
        <Button onClick={() => void copy()}>
          {copied ? <Check className="size-4" /> : <Copy className="size-4" />} {t("common.copy")}
        </Button>
        <Button onClick={download}>
          <Download className="size-4" /> {t("account.recoveryDownload")}
        </Button>
      </div>
      <label className="flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" checked={saved} onChange={(e) => setSaved(e.target.checked)} className="size-4 accent-[var(--color-accent)]" />
        {t("account.recoverySaved")}
      </label>
      {error && <Banner>{error}</Banner>}
      {failed ? (
        <>
          <p className="text-sm text-muted">{t("account.recoveryHandle")}</p>
          <HandleField value={handle} onChange={setHandle} />
        </>
      ) : (
        <p className="text-sm text-muted">{t("account.recoveryCreating", { handle: prepared })}</p>
      )}
      <div className="flex flex-wrap items-center gap-3">
        <Button
          variant="primary"
          disabled={!saved}
          busy={busy}
          onClick={() => {
            const chosen = normalizeHandle(handle);
            if (!chosen) return setError(t("account.handleInvalid"));
            void run(async () => {
              await accountActions.confirmCreated(chosen);
              toast("success", t("account.signedIn", { handle: chosen }));
            }).then((ok) => !ok && setFailed(true));
          }}
        >
          {t("account.recoveryContinue")}
        </Button>
        {failed && <LinkButton onClick={accountActions.cancelCreate}>{t("account.back")}</LinkButton>}
      </div>
    </div>
  );
}

// ------------------------------------------------------------------ signed in

function PasswordDialog({
  title,
  body,
  confirmLabel,
  onSubmit,
  onClose,
}: {
  title: string;
  body: string;
  confirmLabel: string;
  onSubmit: (password: string) => Promise<void>;
  onClose: () => void;
}) {
  const t = useT();
  const [password, setPassword] = useState("");
  const { busy, error, run } = useAction("password");
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (await run(() => onSubmit(password))) onClose();
  };
  return (
    <Dialog open onOpenChange={(o) => !o && onClose()} title={title} description={body}>
      <form onSubmit={(e) => void submit(e)} className="mt-2 flex flex-col gap-3">
        <Field label={t("account.password")}>
          {(id) => (
            <Input id={id} type="password" value={password} onChange={(e) => setPassword(e.target.value)} autoComplete="current-password" autoFocus required />
          )}
        </Field>
        {error && <Banner>{error}</Banner>}
        <div className="flex justify-end gap-2">
          <Button type="button" onClick={onClose}>
            {t("common.cancel")}
          </Button>
          <Button type="submit" variant="primary" busy={busy}>
            {confirmLabel}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

function AccountView() {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const lang = useSettings((s) => s.language);
  const account = useAccount((s) => s.account)!;
  const certChecked = useAccount((s) => s.certChecked);
  const [now] = useState(() => Date.now());
  const [uid, setUid] = useState("");
  const [localUid, setLocalUid] = useState("");
  const [devices, setDevices] = useState<DeviceInfo[] | null>(null);
  const [devicesError, setDevicesError] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<DeviceInfo | null>(null);
  const [renewing, setRenewing] = useState(false);
  const [unlocking, setUnlocking] = useState(false);

  useEffect(() => {
    let alive = true;
    void uidForPublicKey(account.accountKey).then((u) => alive && setUid(u));
    // The local identity that comes back on sign-out.
    void loadOrCreateIdentity()
      .then((id) => uidForPublicKey(id.publicKey))
      .then((u) => alive && setLocalUid(u))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [account.accountKey]);

  const [tick, setTick] = useState(0);
  const load = useCallback(() => setTick((n) => n + 1), []);
  useEffect(() => {
    let alive = true;
    accountActions
      .devices()
      .then((list) => {
        if (!alive) return;
        setDevices(list);
        setDevicesError(null);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        setDevices(null);
        setDevicesError(`${tNow("account.devicesFailed")} ${describeAccountError(e, "session")}`);
      });
    return () => {
      alive = false;
    };
  }, [tick]);

  const thisRevoked = devices?.some((d) => d.device_key === account.deviceKey && d.revoked_at !== null) ?? false;
  const when = (ms: number) => new Date(ms).toLocaleString(lang);
  const warning = certificateWarning(account.expiresAt, now, certChecked);

  return (
    <div>
      <Section title={t("account.signedInAs", { handle: account.handle })}>
        <Field label={t("identity.uid")}>
          {(id) => <Input id={id} readOnly value={uid} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
        </Field>
        <Field label={t("account.accountKey")}>
          {(id) => <Input id={id} readOnly value={account.accountKey} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
        </Field>
        {warning.level === "none" ? (
          <div className="flex flex-wrap items-center gap-2 text-sm text-muted">
            <ShieldCheck className="size-4 shrink-0" />
            <span>{t("account.certExpires", { date: new Date(account.expiresAt).toLocaleDateString(lang) })}</span>
            <Button size="sm" onClick={() => setRenewing(true)}>
              {t("account.renew")}
            </Button>
          </div>
        ) : (
          <div data-testid="cert-warning" role="alert" className="flex flex-col gap-2 rounded-lg border border-warn/50 px-3 py-2 text-sm text-warn">
            <div className="flex gap-2">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" />
              <span>
                {t(warning.level === "expired" ? "account.certExpiredWarn" : "account.certExpiringWarn", {
                  date: new Date(account.expiresAt).toLocaleDateString(lang),
                })}
              </span>
            </div>
            <Button size="sm" variant="primary" className="self-start" onClick={() => setRenewing(true)}>
              {t("account.renewWithPassword")}
            </Button>
          </div>
        )}
      </Section>

      <Section title={t("account.devices")}>
        {thisRevoked && <Banner>{t("account.thisRevoked")}</Banner>}
        {devicesError && <Banner>{devicesError}</Banner>}
        <ul className="flex flex-col gap-1.5" aria-label={t("account.devices")}>
          {devices?.map((d) => {
            const mine = d.device_key === account.deviceKey;
            const revoked = d.revoked_at !== null;
            return (
              <li key={d.device_key} data-device={d.name} className="flex items-center gap-3 rounded-lg border border-line px-3 py-2">
                <Smartphone className="size-4 shrink-0 text-muted" />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2 text-sm">
                    <span className={cn("truncate font-medium", revoked && "text-subtle line-through")}>{d.name}</span>
                    {mine && <span className="rounded bg-accent-soft px-1.5 text-xs text-accent">{t("account.thisDevice")}</span>}
                    {revoked && <span className="rounded bg-danger-soft px-1.5 text-xs text-danger">{t("account.revoked")}</span>}
                  </div>
                  <div className="text-xs text-subtle">{t("account.lastSeen", { when: when(d.last_seen) })}</div>
                </div>
                {!mine && !revoked && (
                  <Button size="sm" variant="secondary" onClick={() => setRevoking(d)}>
                    {t("account.revoke")}
                  </Button>
                )}
              </li>
            );
          })}
        </ul>
        <div>
          <Button size="sm" onClick={load}>
            <RefreshCw className="size-4" /> {t("account.refresh")}
          </Button>
        </div>
      </Section>

      <TeamspeakSection onUnlock={() => setUnlocking(true)} />

      <ChangePassword />

      <Section title={t("account.signOut")}>
        <p className="text-sm text-muted">{t("account.signOutHint", { uid: localUid })}</p>
        <div>
          <Button
            onClick={() =>
              void accountActions.signOut().then(() => toast("success", t("account.signedOut")))
            }
          >
            <LogOut className="size-4" /> {t("account.signOut")}
          </Button>
        </div>
      </Section>

      {revoking && (
        <PasswordDialog
          title={t("account.revokeTitle", { name: revoking.name })}
          body={t("account.revokeBody")}
          confirmLabel={t("account.revoke")}
          onClose={() => setRevoking(null)}
          onSubmit={async (password) => {
            await accountActions.revoke(revoking.device_key, password);
            toast("success", t("account.deviceRevoked"));
            load();
          }}
        />
      )}
      {unlocking && (
        <PasswordDialog
          title={t("ts.unlockTitle")}
          body={t("ts.unlockBody")}
          confirmLabel={t("ts.unlock")}
          onClose={() => setUnlocking(false)}
          onSubmit={async (password) => {
            await accountActions.unlockVault(password);
            toast("success", t("ts.unlocked"));
          }}
        />
      )}
      {renewing && (
        <PasswordDialog
          title={t("account.renewTitle")}
          body={t("account.renewBody")}
          confirmLabel={t("account.renew")}
          onClose={() => setRenewing(false)}
          onSubmit={async (password) => {
            await accountActions.renew(password);
            toast("success", t("account.renewed"));
            load();
          }}
        />
      )}
    </div>
  );
}

function ChangePassword() {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const { busy, error, setError, run } = useAction("password");
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const bad = checkNewPassword(password, confirm);
    if (bad) return setError(bad);
    if (await run(() => accountActions.changePassword({ password: current, newPassword: password }))) {
      setCurrent("");
      setPassword("");
      setConfirm("");
      toast("success", t("account.passwordChanged"));
    }
  };
  return (
    <Section title={t("account.changePassword")}>
      <form onSubmit={(e) => void submit(e)} className="flex flex-col gap-3" aria-label={t("account.changePassword")}>
        <Field label={t("account.currentPassword")}>
          {(id) => <Input id={id} type="password" value={current} onChange={(e) => setCurrent(e.target.value)} autoComplete="current-password" required />}
        </Field>
        <NewPasswordFields password={password} onPassword={setPassword} confirm={confirm} onConfirm={setConfirm} label={t("account.newPassword")} />
        {error && <Banner>{error}</Banner>}
        <Button type="submit" busy={busy} className="self-start">
          {t("account.changePassword")}
        </Button>
      </form>
    </Section>
  );
}
