import { DesktopStartupSettings } from "./DesktopStartupSettings";
import { SoundSettings } from "./SoundSettings";
import { Bell, Volume2, Check, CloudCog, Copy, Download, Languages, Monitor, Moon, Palette, Settings as SettingsIcon, SlidersHorizontal, Sun, Upload, UserRound } from "lucide-react";
import { exportIdentityBackup } from "../state/identity-backup";
import { useEffect, useRef, useState } from "react";
import { tNow, useT } from "../i18n";
import { cn } from "../lib/cn";
import { accountKeyOf, uidForPublicKey } from "../net/identity";
import { describeAccountError } from "../connect/errors";
import { validNickname } from "../net/nickname";
import { useAccountIdentity } from "../state/identity";
import { accountActions, useAccount } from "../state/account";
import { AccountTab } from "./AccountSettings";
import { isDesktop } from "../platform";
import { notificationsPermission, requestNotifications, type NotificationsPermission } from "../platform/notify";
import { acceleratorFromEvent, acceleratorParts } from "../platform/desktop";
import { controller, describeVoiceError, identityFailure } from "../state/controller";
import { useSettings, type Language, type Theme } from "../state/settings";
import { useSession, useUi, useVoice, type SettingsTab } from "../state/stores";
import { VoiceError } from "../voice/engine";
import { keyLabel } from "./hooks";
import { Button, Field, Input, Section, Segmented, Select, Slider, Switch } from "./kit";
import { AlphaBadge } from "./BugReport";
import { SettingsPageLayout, type SettingsPage } from "./SettingsLayout";
import { VersionLabel } from "./Version";
import { InputSensitivity } from "./InputSensitivity";
import { useMicPreview } from "./use-mic-preview";

const DEFAULT = "__default";

/** The settings section: the gear opens it on the last tab, the account avatar on the identity tab. */
export function SettingsSection({ card }: { card?: boolean }) {
  const t = useT();
  const tab = useUi((s) => s.settingsTab);
  const setTab = useUi((s) => s.setSettingsTab);
  // What the phones' list shows beside each page, where it is a word or two.
  const inputMode = useSettings((s) => s.audio.inputMode);
  const theme = useSettings((s) => s.theme);
  const language = useSettings((s) => s.language);
  const nickname = useAccountIdentity((s) => s.nickname);
  const handle = useAccount((s) => s.account?.handle);
  const tabs: Array<SettingsPage<SettingsTab>> = [
    { id: "audio", label: t("settings.audio"), icon: <SlidersHorizontal className="size-4" />, value: t(inputMode === "ptt" ? "audio.ptt" : "audio.vad") },
    { id: "notifications", label: t("settings.notifications"), icon: <Bell className="size-4" /> },
    { id: "sounds", label: t("settings.sounds"), icon: <Volume2 className="size-4" /> },
    { id: "appearance", label: t("settings.appearance"), icon: <Palette className="size-4" />, value: t(`appearance.${theme}`) },
    { id: "identity", label: t("settings.identity"), icon: <UserRound className="size-4" />, value: nickname || undefined, group: 1 },
    { id: "account", label: t("settings.account"), icon: <CloudCog className="size-4" />, value: handle ? `@${handle}` : undefined, group: 1 },
    { id: "language", label: t("settings.language"), icon: <Languages className="size-4" />, value: language === "pl" ? "Polski" : "English" },
  ];
  const footer = (
    <div className="flex flex-col items-center gap-1.5 pt-1">
      <VersionLabel />
      <AlphaBadge />
    </div>
  );
  return (
    <SettingsPageLayout title={t("settings.title")} icon={<SettingsIcon />} pages={tabs} value={tab} onValueChange={setTab} card={card} footer={footer}>
      {(page) =>
        page === "audio" ? <AudioTab />
        : page === "notifications" ? <NotificationsTab />
        : page === "sounds" ? <SoundSettings />
        : page === "appearance" ? <AppearanceTab />
        : page === "identity" ? <IdentityTab />
        : page === "account" ? <AccountTab />
        : <LanguageTab />}
    </SettingsPageLayout>
  );
}

// -------------------------------------------------------------------- audio

function LevelMeter({ level, label }: { level: number; label: string }) {
  return (
    <div
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(level * 100)}
      className="h-3 w-full overflow-hidden rounded-full bg-active"
    >
      <div className="h-full rounded-full bg-ok transition-[width] duration-75" style={{ width: `${Math.round(level * 100)}%` }} />
    </div>
  );
}

/** Records a key combination for a global (desktop) shortcut. */
function ShortcutField({ label, value, onChange }: { label: string; value: string | null; onChange: (v: string | null) => void }) {
  const t = useT();
  const [capturing, setCapturing] = useState(false);
  useEffect(() => {
    if (!capturing) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.code === "Escape") return setCapturing(false);
      const accelerator = acceleratorFromEvent(e);
      if (!accelerator) return; // only a modifier so far
      onChange(accelerator);
      setCapturing(false);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capturing, onChange]);
  return (
    <Field label={label}>
      {() => (
        <div className="flex items-center gap-2">
          <Button onClick={() => setCapturing(true)} className={cn("min-w-40", capturing && "border-accent text-accent")}>
            {capturing
              ? t("audio.pressKey")
              : value
                ? acceleratorParts(value)
                    .map((p) => keyLabel(p))
                    .join(" + ")
                : t("audio.shortcutSet")}
          </Button>
          {value && !capturing && (
            <Button variant="ghost" onClick={() => onChange(null)}>
              {t("audio.clearKey")}
            </Button>
          )}
        </div>
      )}
    </Field>
  );
}

function AudioTab() {
  const t = useT();
  const audio = useSettings((s) => s.audio);
  const setAudio = useSettings((s) => s.setAudio);
  const desktop = useSettings((s) => s.desktop);
  const setDesktop = useSettings((s) => s.setDesktop);
  const onDesktop = isDesktop();
  const devices = useVoice((s) => s.devices);
  const engineLevel = useVoice((s) => s.level);
  const voiceState = useVoice((s) => s.state);
  const gateUnsupported = useVoice((s) => s.inputGateSupport === false);
  const muted = useVoice((s) => s.muted || s.deafened);
  const [capturing, setCapturing] = useState(false);
  const engine = controller.voice;
  const preview = useMicPreview(engine);
  // Gwar's own suppression has a strength: native on the desktop, an AudioWorklet in the browser.
  const noiseStrength = audio.noiseSuppression && (
    <Field label={t("audio.noiseStrength")}>
      {() => (
        <div className="flex items-center gap-3">
          <Slider
            value={audio.noiseStrength}
            min={10}
            max={100}
            step={5}
            onChange={(noiseStrength) => setAudio({ noiseStrength })}
            label={t("audio.noiseStrength")}
          />
          <span className="w-12 shrink-0 text-right text-sm tabular-nums text-muted">{audio.noiseStrength}%</span>
        </div>
      )}
    </Field>
  );

  useEffect(() => {
    void engine.listDevices().then((d) => useVoice.getState().set({ devices: d }));
  }, [engine]);

  useEffect(() => {
    if (!capturing) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.code !== "Escape") setAudio({ pttKey: e.code });
      setCapturing(false);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capturing, setAudio]);

  const withDefault = (list: Array<{ id: string; label: string }>) => [
    { value: DEFAULT, label: t("audio.systemDefault") },
    ...list.map((d) => ({ value: d.id, label: d.label })),
  ];
  const inCall = voiceState !== "idle";
  const level = muted ? 0 : inCall ? engineLevel : preview.level;
  const automatic = gateUnsupported || audio.automaticSensitivity;

  return (
    <div>
      <Section title={t("audio.devices")}>
        <Field label={t("audio.input")}>
          {(id) => (
            <Select
              id={id}
              value={audio.inputDeviceId && devices.inputs.some((d) => d.id === audio.inputDeviceId) ? audio.inputDeviceId : DEFAULT}
              onValueChange={(v) => setAudio({ inputDeviceId: v === DEFAULT ? null : v })}
              options={withDefault(devices.inputs)}
            />
          )}
        </Field>
        <Field label={t("audio.output")} hint={engine.capabilities.outputDeviceSelection ? undefined : t("audio.outputUnsupported")}>
          {(id) => (
            <Select
              id={id}
              disabled={!engine.capabilities.outputDeviceSelection}
              value={audio.outputDeviceId && devices.outputs.some((d) => d.id === audio.outputDeviceId) ? audio.outputDeviceId : DEFAULT}
              onValueChange={(v) => setAudio({ outputDeviceId: v === DEFAULT ? null : v })}
              options={withDefault(devices.outputs)}
            />
          )}
        </Field>
        <Field label={t("audio.masterVolume")}>
          {() => (
            <div className="flex items-center gap-3">
              <Slider
                value={Math.round(audio.masterVolume * 100)}
                min={0}
                max={150}
                step={5}
                onChange={(v) => setAudio({ masterVolume: v / 100 })}
                label={t("audio.masterVolume")}
              />
              <span className="w-12 shrink-0 text-right text-sm tabular-nums text-muted">{Math.round(audio.masterVolume * 100)}%</span>
            </div>
          )}
        </Field>
      </Section>

      <Section title={t("audio.inputMode")}>
        <Segmented
          label={t("audio.inputMode")}
          value={audio.inputMode}
          onChange={(inputMode) => setAudio({ inputMode })}
          options={[
            { value: "vad", label: t("audio.vad") },
            { value: "ptt", label: t("audio.ptt") },
          ]}
        />
        <p className="text-xs text-subtle">{t(audio.inputMode === "vad" ? "audio.vadHelp" : "audio.pttModeHint")}</p>
        <div data-input-mode-options className="grid grid-rows-[44px_44px_minmax(32px,auto)] gap-2">
          {audio.inputMode === "vad" ? <>
            <Switch checked={automatic} disabled={gateUnsupported} onCheckedChange={(automaticSensitivity) => setAudio({ automaticSensitivity })} label={t("audio.automaticSensitivity")} description={t(gateUnsupported ? "audio.manualUnsupported" : "audio.automaticSensitivityHint")} />
            {automatic ? <div className="flex items-center"><LevelMeter level={level} label={t("audio.level")} /></div> : <InputSensitivity level={level} threshold={audio.vadThreshold} onChange={(vadThreshold) => setAudio({ vadThreshold })} />}
            <div className="flex items-center">
              {preview.needsPermission ? <Button onClick={preview.request} className="h-8 self-start text-xs">{t("audio.previewEnable")}</Button> : !automatic && !audio.noiseSuppression ? <p className="text-xs text-subtle">{t("audio.noiseSuggestion")}</p> : null}
            </div>
          </> : <>
            <div className="flex items-center gap-2">
              <Button disabled={gateUnsupported} onClick={() => setCapturing(true)} className={cn("min-w-40", capturing && "border-accent text-accent")}>
                {capturing ? t("audio.pressKey") : audio.pttKey ? keyLabel(audio.pttKey) : t("audio.setKey")}
              </Button>
              {audio.pttKey && !capturing && <Button variant="ghost" onClick={() => setAudio({ pttKey: null })}>{t("audio.clearKey")}</Button>}
            </div>
            <p className="self-center text-xs text-subtle">{t(gateUnsupported ? "audio.pttUnsupported" : audio.pttKey ? onDesktop ? "audio.pttHelpGlobal" : "audio.pttHelp" : "audio.pttNoKey")}</p>
            <div aria-hidden="true" />
          </>}
        </div>
        {preview.error != null && <p role="alert" className="text-sm text-danger">{describeVoiceError(preview.error instanceof VoiceError ? preview.error : new VoiceError("mic_failed", String(preview.error)))}</p>}
      </Section>

      <Section title={t("audio.processing")}>
        <Switch
          checked={audio.noiseSuppression}
          onCheckedChange={(v) => setAudio({ noiseSuppression: v })}
          label={t("audio.noiseSuppression")}
          description={t("audio.noiseSuppressionHint")}
        />
        {noiseStrength}
        <Switch
          checked={audio.echoCancellation}
          onCheckedChange={(v) => setAudio({ echoCancellation: v })}
          label={t("audio.echoCancellation")}
          description={t("audio.echoCancellationHint")}
        />
        <Switch
          checked={audio.autoGainControl}
          onCheckedChange={(v) => setAudio({ autoGainControl: v })}
          label={t("audio.autoGain")}
          description={t("audio.autoGainHint")}
        />
      </Section>

      {onDesktop && (
        <Section title={t("audio.globalShortcuts")}>
          <ShortcutField label={t("audio.muteShortcut")} value={desktop.muteShortcut} onChange={(v) => setDesktop({ muteShortcut: v })} />
          <ShortcutField label={t("audio.deafenShortcut")} value={desktop.deafenShortcut} onChange={(v) => setDesktop({ deafenShortcut: v })} />
          <p className="text-xs text-subtle">{t("audio.shortcutHint")}</p>
        </Section>
      )}
    </div>
  );
}

// --------------------------------------------------------------- appearance

function NotificationsTab() {
  const t = useT();
  const prefs = useSettings((s) => s.notifications);
  const setPrefs = useSettings((s) => s.setNotifications);
  const [permission, setPermission] = useState<NotificationsPermission | null>(null);

  useEffect(() => {
    let alive = true;
    void notificationsPermission().then((p) => alive && setPermission(p));
    return () => {
      alive = false;
    };
  }, []);

  // The first time something is switched on the system is asked, from this click.
  const ask = async () => {
    const granted = await requestNotifications();
    setPermission(granted ? "granted" : await notificationsPermission());
    return granted;
  };
  const toggle = (key: keyof typeof prefs) => async (on: boolean) => {
    setPrefs({ [key]: on });
    if (on && permission !== "granted") await ask();
  };

  return (
    <div>
      <Section title={t("notify.when")}>
        <Switch checked={prefs.mentions} onCheckedChange={(v) => void toggle("mentions")(v)} label={t("notify.mentions")} description={t("notify.mentionsHint")} />
        <Switch
          checked={prefs.privateMessages}
          onCheckedChange={(v) => void toggle("privateMessages")(v)}
          label={t("notify.dms")}
          description={t("notify.dmsHint")}
        />
        <Switch checked={prefs.allMessages} onCheckedChange={(v) => void toggle("allMessages")(v)} label={t("notify.all")} description={t("notify.allHint")} />
        <p className="text-xs text-subtle">{t("notify.backgroundOnly")}</p>
      </Section>
      <Section title={t("notify.permission")}>
        {permission === "granted" ? (
          <p className="flex items-center gap-2 text-sm text-ok">
            <Check className="size-4" /> {t("notify.granted")}
          </p>
        ) : permission === "denied" ? (
          <p className="text-sm text-muted">{isDesktop() ? t("notify.deniedDesktop") : t("notify.denied")}</p>
        ) : (
          <div className="flex items-center gap-3">
            <Button onClick={() => void ask()}>{t("notify.allow")}</Button>
            <span className="text-xs text-subtle">{t("notify.allowHint")}</span>
          </div>
        )}
      </Section>
    </div>
  );
}

function AppearanceTab() {
  const t = useT();
  const theme = useSettings((s) => s.theme);
  const setTheme = useSettings((s) => s.setTheme);
  const compact = useSettings((s) => s.compact);
  const setCompact = useSettings((s) => s.setCompact);
  const desktop = useSettings((s) => s.desktop);
  const setDesktop = useSettings((s) => s.setDesktop);
  return (
    <div>
      <Section title={t("appearance.theme")}>
        <Segmented<Theme>
          label={t("appearance.theme")}
          value={theme}
          onChange={setTheme}
          options={[
            { value: "system", label: t("appearance.system"), icon: <Monitor className="size-4" /> },
            { value: "dark", label: t("appearance.dark"), icon: <Moon className="size-4" /> },
            { value: "light", label: t("appearance.light"), icon: <Sun className="size-4" /> },
          ]}
        />
      </Section>
      <Section title={t("appearance.channelList")}>
        <Switch checked={compact} onCheckedChange={setCompact} label={t("appearance.compact")} description={t("appearance.compactHint")} />
      </Section>
      {isDesktop() && (
        <Section title={t("desktop.window")}>
          <DesktopStartupSettings />
          <Switch
            checked={desktop.closeToTray}
            onCheckedChange={(closeToTray) => setDesktop({ closeToTray })}
            label={t("desktop.closeToTray")}
            description={t("desktop.closeToTrayHint")}
          />
        </Section>
      )}
    </div>
  );
}

// ----------------------------------------------------------------- identity

function IdentityTab() {
  const t = useT();
  const toast = useUi((s) => s.toast);
  const me = useSession((s) => (s.me ? s.clients[s.me.session] : undefined));
  const online = useSession((s) => s.phase === "online");
  const [uid, setUid] = useState<string>("");
  const [publicKey, setPublicKey] = useState<string>("");
  const [copied, setCopied] = useState(false);
  const globalNickname = useAccountIdentity((s) => s.nickname);
  const [nicknameDraft, setNicknameDraft] = useState({ source: globalNickname, value: globalNickname });
  const nickname = nicknameDraft.source === globalNickname ? nicknameDraft.value : globalNickname;
  const setNickname = (value: string) => setNicknameDraft({ source: globalNickname, value });
  const [nicknameError, setNicknameError] = useState<string | null>(null);
  const [away, setAway] = useState(me?.away ?? "");
  // The away message as shown when opened (or last saved): only an edit made here is sent, as
  // the shown one may be a cleaned copy of the server's.
  const [shownAway, setShownAway] = useState(away);
  const [busy, setBusy] = useState(false);
  const file = useRef<HTMLInputElement>(null);

  const connected = useAccount((s) => s.account);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    let alive = true;
    controller
      .getIdentity()
      .then(async (id) => {
        const u = await uidForPublicKey(accountKeyOf(id));
        if (!alive) return;
        setPublicKey(accountKeyOf(id));
        setUid(u);
      })
      .catch((e: unknown) => {
        if (!alive) return;
        const failure = identityFailure(e);
        useUi.getState().toast("error", failure === "identity_storage" ? tNow("settings.identityUnavailable") : tNow(`err.connect.${failure}`));
      });
    return () => {
      alive = false;
    };
  }, [version, connected?.accountKey]);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(uid);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      toast("error", t("toast.copyFailed"));
    }
  };

  const exportIdentity = async () => {
    try { await exportIdentityBackup(); }
    catch { toast("error", t("identity.exportFailed")); }
  };

  const importIdentity = async (f: File | undefined) => {
    if (!f) return;
    try {
      await controller.importIdentity(await f.text());
      useSettings.getState().setOnboarding({ completed: true });
      useUi.getState().dismissToast("identity-load");
      setVersion((v) => v + 1);
      toast("success", t("identity.imported"));
    } catch {
      toast("error", t("identity.importFailed"));
    }
    if (file.current) file.current.value = "";
  };

  const saveProfile = async () => {
    setBusy(true);
    try {
      if (away !== shownAway) await controller.updateProfile({ away: away.trim() });
      setShownAway(away);
      toast("success", t("common.saved"));
    } catch {
      toast("error", t("err.req.internal"));
    }
    setBusy(false);
  };

  const saveNickname = async () => {
    setBusy(true);
    setNicknameError(null);
    try {
      await accountActions.changeNickname(nickname);
      toast("success", t("common.saved"));
    } catch (e) {
      setNicknameError(describeAccountError(e, "session"));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <Section title={t("nickname.identityTitle")}>
        <Field label={t("connect.nickname")} hint={t("nickname.globalHint")}>
          {(id) => <Input id={id} value={nickname} onChange={(e) => setNickname(e.target.value)} autoComplete="nickname" />}
        </Field>
        {nicknameError && <p role="alert" className="text-sm text-danger">{nicknameError}</p>}
        <div>
          <Button variant="primary" size="sm" busy={busy} onClick={() => void saveNickname()} disabled={!validNickname(nickname) || !globalNickname}>
            {t("common.save")}
          </Button>
        </div>
      </Section>
      {online && (
        <Section title={t("identity.profile")}>
          <Field label={t("identity.away")} hint={t("identity.awayHint")}>
            {(id) => <Input id={id} value={away} onChange={(e) => setAway(e.target.value)} maxLength={80} />}
          </Field>
          <div>
            <Button variant="primary" size="sm" busy={busy} onClick={() => void saveProfile()}>
              {t("common.save")}
            </Button>
          </div>
        </Section>
      )}
      <Section title={t("identity.title")}>
        <p className="text-sm text-muted">
          {connected ? t("identity.connectNote", { handle: connected.handle }) : t("identity.description")}
        </p>
        <Field label={t("identity.uid")}>
          {(id) => (
            <div className="flex gap-2">
              <Input id={id} readOnly value={uid} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />
              <Button onClick={() => void copy()} className="shrink-0" aria-label={t("common.copy")}>
                {copied ? <Check className="size-4" /> : <Copy className="size-4" />}
              </Button>
            </div>
          )}
        </Field>
        <Field label={connected ? t("identity.accountKey") : t("identity.publicKey")}>
          {(id) => <Input id={id} readOnly value={publicKey} className="font-mono text-xs" onFocus={(e) => e.currentTarget.select()} />}
        </Field>
      </Section>
      {!connected && (
      <Section title={t("identity.backup")}>
        <p className="text-sm text-muted">{t("identity.backupHint")}</p>
        <div className="flex flex-wrap gap-2">
          <Button onClick={() => void exportIdentity()}>
            <Download className="size-4" /> {t("identity.export")}
          </Button>
          <Button onClick={() => file.current?.click()}>
            <Upload className="size-4" /> {t("identity.import")}
          </Button>
          <input ref={file} type="file" accept="application/json,.json" className="hidden" onChange={(e) => void importIdentity(e.target.files?.[0])} />
        </div>
        <p className="text-xs text-warn">{t("identity.importWarning")}</p>
      </Section>
      )}
    </div>
  );
}

// ----------------------------------------------------------------- language

function LanguageTab() {
  const t = useT();
  const language = useSettings((s) => s.language);
  const setLanguage = useSettings((s) => s.setLanguage);
  return (
    <Section title={t("settings.language")}>
      <Segmented<Language>
        label={t("settings.language")}
        value={language}
        onChange={setLanguage}
        options={[
          { value: "pl", label: "Polski" },
          { value: "en", label: "English" },
        ]}
      />
    </Section>
  );
}
