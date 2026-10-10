import { AlertTriangle, CloudCog, Menu, Mic, ShieldAlert, Ticket } from "lucide-react";
import { useEffect, useState, type FormEvent } from "react";
import { useT } from "../i18n";
import { pageContextFromLocation, sameOriginUrl } from "../net/address";
import { parseInviteLink } from "../net/invite";
import { controller, describeBan } from "../state/controller";
import { isDesktop } from "../platform";
import { useSettings, type ServerKind } from "../state/settings";
import { useAccount } from "../state/account";
import { useConnectUi, useSession, useUi } from "../state/stores";
import { Avatar, Button, Field, IconButton, Input, Segmented, Switch } from "./kit";
import { TeamSpeakBadge } from "./badges";
import { useIsMobile } from "../lib/media";
import { resolveIdentity } from "../connect/ts-list";
import { useTsIdentities } from "./hooks";
import { IdentityPicker } from "./IdentityPicker";

/**
 * The web app is not a server by default: it is hosted separately and connects
 * to any server. Only when the page's own origin also runs a Gwar server
 * (`/health` answers "ok"; a static host or the Vite dev server answers with
 * something else) is "the server I came from" a natural default.
 */
async function probeSameOriginServer(): Promise<string | null> {
  if (!/^https?:$/.test(window.location.protocol)) return null;
  try {
    const r = await fetch("/health", { signal: AbortSignal.timeout(2000) });
    if (!r.ok || (await r.text()).trim() !== "ok") return null;
    return sameOriginUrl(pageContextFromLocation()).replace(/^wss?:\/\//, "").replace(/\/ws$/, "");
  } catch {
    return null;
  }
}

export function ConnectScreen() {
  const t = useT();
  const isMobile = useIsMobile();
  const lastAddress = useSettings((s) => s.lastAddress);
  const lastNickname = useSettings((s) => s.lastNickname);
  const lastKind = useSettings((s) => s.lastKind);
  const desktop = isDesktop();
  const bookmarks = useSettings((s) => s.bookmarks);
  const { busy, error, needPassword, serverName, invite } = useConnectUi();
  const closeReason = useSession((s) => s.closeReason);
  const setDrawer = useUi((s) => s.setDrawer);
  const openDialog = useUi((s) => s.openDialog);
  const account = useAccount((s) => s.account);
  const [kind, setKind] = useState<ServerKind>(desktop && !invite?.server ? lastKind : "vc");
  const [address, setAddress] = useState(invite?.server ?? lastAddress);
  const [nickname, setNickname] = useState(lastNickname);
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  // TeamSpeak: with more than one identity the person chooses; null means the default one.
  const tsList = useTsIdentities();
  const [identity, setIdentity] = useState<string | null>(null);
  const pickIdentity = kind === "teamspeak" && !!tsList && tsList.identities.length >= 2;

  // An invite link without a server means "the server on this page's own origin".
  const inviteWithoutServer = !!invite && !invite.server;
  useEffect(() => {
    if (kind !== "vc" || (lastAddress && !inviteWithoutServer)) return;
    let alive = true;
    void probeSameOriginServer().then((found) => {
      if (alive && found) setAddress((current) => (inviteWithoutServer ? found : current || found));
    });
    return () => {
      alive = false;
    };
  }, [lastAddress, kind, inviteWithoutServer]);

  // Opened from an invite link: the server it names replaces whatever was remembered.
  const [seenInvite, setSeenInvite] = useState(invite);
  if (invite !== seenInvite) {
    setSeenInvite(invite);
    if (invite?.server) {
      setKind("vc");
      setAddress(invite.server);
    }
  }

  // A pasted invite link becomes the server address plus the invite.
  const onAddressChange = (value: string) => {
    const pasted = kind === "vc" ? parseInviteLink(value) : null;
    if (pasted) {
      useConnectUi.getState().set({ invite: pasted });
      setAddress(pasted.server ?? "");
      return;
    }
    setAddress(value);
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (busy) return;
    void controller.connectInteractive(
      {
        kind,
        address,
        nickname,
        password,
        invite: kind === "vc" ? invite?.code : undefined,
        identity: pickIdentity ? (resolveIdentity(tsList, identity) ?? undefined) : undefined,
      },
      { remember },
    );
  };

  const notice = closeNotice(closeReason, t);

  return (
    <main className="relative flex min-w-0 flex-1 flex-col items-center overflow-y-auto bg-surface">
      {isMobile && (
        <div className="flex h-12 w-full shrink-0 items-center px-2">
          <IconButton label={t("nav.servers")} onClick={() => setDrawer(true)}>
            <Menu className="size-5" />
          </IconButton>
        </div>
      )}
      <div className="flex w-full flex-1 flex-col items-center justify-center gap-6 px-4 py-8">
        <form
          onSubmit={submit}
          className="anim-pop flex w-full max-w-sm flex-col gap-4 rounded-2xl border border-line bg-side p-6 shadow-pop"
        >
          <div className="flex flex-col items-center gap-2 pb-1 text-center">
            <div className="flex size-12 items-center justify-center rounded-2xl bg-accent text-accent-fg">
              <Mic className="size-6" />
            </div>
            <h1 className="text-xl font-semibold tracking-tight">{t("connect.title")}</h1>
            <p className="text-sm text-muted">{t("connect.subtitle")}</p>
          </div>

          {invite && kind === "vc" && (
            <div className="flex items-start gap-2 rounded-lg bg-accent-soft px-3 py-2 text-sm text-fg">
              <Ticket className="mt-0.5 size-4 shrink-0 text-accent" />
              <span className="min-w-0 flex-1">
                {t("connect.invited")}
                <button
                  type="button"
                  onClick={() => useConnectUi.getState().set({ invite: null })}
                  className="ml-2 cursor-pointer text-xs text-muted underline hover:text-fg"
                >
                  {t("connect.inviteForget")}
                </button>
              </span>
            </div>
          )}

          {notice && (
            <div role="alert" className="flex gap-2 rounded-lg bg-danger-soft px-3 py-2 text-sm text-danger">
              <ShieldAlert className="mt-0.5 size-4 shrink-0" />
              <span>{notice}</span>
            </div>
          )}

          <div className="flex flex-col gap-1.5">
            <Segmented<ServerKind>
              label={t("connect.kind")}
              value={kind}
              onChange={setKind}
              options={[
                { value: "vc", label: t("connect.kindVc") },
                { value: "teamspeak", label: t("connect.kindTs"), disabled: !desktop, title: desktop ? undefined : t("connect.tsWebOnly") },
              ]}
            />
            {!desktop && <p className="text-xs text-subtle">{t("connect.tsWebOnly")}</p>}
          </div>

          <Field label={t("connect.address")} hint={kind === "teamspeak" ? t("connect.tsAddressHint") : t("connect.addressHint")}>
            {(id) => (
              <Input
                id={id}
                value={address}
                onChange={(e) => onAddressChange(e.target.value)}
                placeholder={kind === "teamspeak" ? "ts.example.com" : "voice.example.com"}
                autoCapitalize="none"
                autoCorrect="off"
                spellCheck={false}
                required
                autoFocus={!address}
              />
            )}
          </Field>
          <Field label={t("connect.nickname")}>
            {(id) => (
              <Input
                id={id}
                value={nickname}
                onChange={(e) => setNickname(e.target.value)}
                placeholder={t("connect.nicknamePlaceholder")}
                maxLength={32}
                required
                autoComplete="nickname"
                autoFocus={!!address && !nickname}
              />
            )}
          </Field>
          {(needPassword || kind === "teamspeak") && (
            <Field
              label={kind === "teamspeak" ? t("connect.tsPassword") : t("connect.password")}
              hint={needPassword && serverName ? t("connect.passwordHint", { server: serverName }) : undefined}
            >
              {(id) => (
                <Input
                  id={id}
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="current-password"
                  autoFocus={needPassword}
                  required={needPassword}
                />
              )}
            </Field>
          )}

          {pickIdentity && <IdentityPicker list={tsList} value={identity} onChange={setIdentity} />}

          <Switch checked={remember} onCheckedChange={setRemember} label={t("connect.remember")} />

          {error && (
            <div role="alert" className="flex gap-2 rounded-lg bg-danger-soft px-3 py-2 text-sm text-danger">
              <AlertTriangle className="mt-0.5 size-4 shrink-0" />
              <span>{error}</span>
            </div>
          )}

          <Button type="submit" variant="primary" busy={busy} className="w-full">
            {busy ? t("connect.connecting") : t("connect.submit")}
          </Button>
        </form>

        <button
          type="button"
          onClick={() => openDialog({ kind: "settings", tab: "account" })}
          className="t -mt-2 flex w-full max-w-sm cursor-pointer items-center gap-3 rounded-lg border border-line px-3 py-2 text-left hover:bg-hover"
        >
          <CloudCog className="size-4 shrink-0 text-muted" />
          <span className="min-w-0 flex-1">
            <span className="block truncate text-sm font-medium">
              {account ? t("account.signedInAs", { handle: account.handle }) : t("account.connectEntry")}
            </span>
            {!account && <span className="block truncate text-xs text-muted">{t("account.connectEntryHint")}</span>}
          </span>
        </button>

        {bookmarks.length > 0 && (
          <section aria-label={t("connect.saved")} className="w-full max-w-sm">
            <h2 className="mb-2 px-1 text-xs font-medium text-subtle">{t("connect.saved")}</h2>
            <ul className="flex flex-col gap-1">
              {bookmarks.map((b) => (
                <li key={b.id}>
                  <button
                    onClick={() =>
                      void controller.connectInteractive(
                        { kind: b.kind, address: b.address, nickname: b.nickname, password: b.password, identity: b.identity },
                        { remember: false },
                      )
                    }
                    disabled={busy}
                    className="t flex w-full cursor-pointer items-center gap-3 rounded-lg border border-line px-3 py-2 text-left hover:bg-hover disabled:opacity-50"
                  >
                    <Avatar name={b.name} seed={b.address} size={32} square />
                    <span className="min-w-0 flex-1">
                      <span className="flex items-center gap-1.5">
                        <span className="truncate text-sm font-medium">{b.name}</span>
                        {b.kind === "teamspeak" && <TeamSpeakBadge />}
                      </span>
                      <span className="block truncate text-xs text-muted">
                        {b.nickname} · {b.address}
                      </span>
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </main>
  );
}

function closeNotice(reason: ReturnType<typeof useSession.getState>["closeReason"], t: ReturnType<typeof useT>): string | null {
  if (!reason) return null;
  if (reason.kind === "server") {
    const r = reason.reason;
    if (r.kind === "kicked") return r.reason ? t("close.kickedReason", { by: r.by, reason: r.reason }) : t("close.kicked", { by: r.by });
    if (r.kind === "removed") return t("close.removed", { by: r.by });
    if (r.kind === "replaced") return t("close.replaced");
    if (r.kind === "banned") return describeBan({ until: r.until, reason: r.reason }, r.by);
    return null;
  }
  if (reason.kind === "error") return t("close.lost");
  return null;
}
