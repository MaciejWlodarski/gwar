import * as Tabs from "@radix-ui/react-tabs";
import { Bell, Check, CloudCog, Copy, Download, Languages, Monitor, Moon, Palette, SlidersHorizontal, Sun, Upload, UserRound } from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { tNow, useT } from "../i18n";
import { cn } from "../lib/cn";
import { accountKeyOf, uidForPublicKey } from "../net/identity";
import { useAccount } from "../state/account";
import { AccountTab } from "./AccountSettings";
import { isDesktop } from "../platform";
import { notificationsPermission, requestNotifications, type NotificationsPermission } from "../platform/notify";
import { acceleratorFromEvent, acceleratorParts } from "../platform/desktop";
import { controller, describeVoiceError } from "../state/controller";
import { useSettings, type Language, type Theme } from "../state/settings";
import { useSession, useUi, useVoice, type SettingsTab } from "../state/stores";
import { VoiceError } from "../voice/engine";
import { keyLabel } from "./hooks";
import { Button, Dialog, Field, Input, Segmented, Select, Slider, Switch } from "./kit";

const DEFAULT = "__default";

export function SettingsDialog({ tab }: { tab: SettingsTab }) {
  const t = useT();
  const close = useUi((s) => s.closeDialog);
  const [current, setCurrent] = useState<SettingsTab>(tab);
  const tabs: Array<{ id: SettingsTab; label: string; icon: ReactNode }> = [
    { id: "audio", label: t("settings.audio"), icon: <SlidersHorizontal className="size-4" /> },
    { id: "notifications", label: t("settings.notifications"), icon: <Bell className="size-4" /> },
    { id: "appearance", label: t("settings.appearance"), icon: <Palette className="size-4" /> },
    { id: "identity", label: t("settings.identity"), icon: <UserRound className="size-4" /> },
    { id: "account", label: t("settings.account"), icon: <CloudCog className="size-4" /> },
    { id: "language", label: t("settings.language"), icon: <Languages className="size-4" /> },
  ];
  return (
    <Dialog open onOpenChange={(o) => !o && close()} title={t("settings.title")} width="max-w-xl">
      <Tabs.Root value={current} onValueChange={(v) => setCurrent(v as SettingsTab)}>
        <Tabs.List className="-mx-1 mb-4 flex gap-1 overflow-x-auto border-b border-line px-1 pb-2" aria-label={t("settings.title")}>
          {tabs.map((x) => (
            <Tabs.Trigger
              key={x.id}
              value={x.id}
              className="t flex h-8 shrink-0 cursor-pointer items-center gap-2 rounded-md px-3 text-sm text-muted hover:bg-hover hover:text-fg data-[state=active]:bg-active data-[state=active]:text-fg"
            >
              {x.icon}
              {x.label}
            </Tabs.Trigger>
          ))}
        </Tabs.List>
        <Tabs.Content value="audio" className="outline-none">
          <AudioTab />
        </Tabs.Content>
        <Tabs.Content value="notifications" className="outline-none">
          <NotificationsTab />
        </Tabs.Content>
        <Tabs.Content value="appearance" className="outline-none">
          <AppearanceTab />
        </Tabs.Content>
        <Tabs.Content value="identity" className="outline-none">
          <IdentityTab />
        </Tabs.Content>
        <Tabs.Content value="account" className="outline-none">
          <AccountTab />
        </Tabs.Content>
        <Tabs.Content value="language" className="outline-none">
          <LanguageTab />
        </Tabs.Content>
      </Tabs.Root>
    </Dialog>
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

// -------------------------------------------------------------------- audio

function LevelMeter({ level, label }: { level: number; label: string }) {
  return (
    <div
      role="meter"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(level * 100)}
      className="h-2 w-full overflow-hidden rounded-full bg-active"
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
  const [testing, setTesting] = useState(false);
  const [testLevel, setTestLevel] = useState(0);
  const [testError, setTestError] = useState<string | null>(null);
  const [capturing, setCapturing] = useState(false);
  const engine = controller.voice;

  useEffect(() => {
    void engine.listDevices().then((d) => useVoice.getState().set({ devices: d }));
  }, [engine]);

  // The mic test runs while `testing`; changing device or processing flags restarts it.
  useEffect(() => {
    if (!testing) return;
    let cancelled = false;
    let stop: (() => void) | undefined;
    engine
      .startMicTest(setTestLevel)
      .then((fn) => {
        if (cancelled) fn();
        else stop = fn;
      })
      .catch((e: unknown) => {
        setTestError(e instanceof VoiceError ? describeVoiceError(e) : String(e));
        setTesting(false);
      });
    return () => {
      cancelled = true;
      stop?.();
    };
  }, [engine, testing, audio.inputDeviceId, audio.noiseSuppression, audio.echoCancellation, audio.autoGainControl]);

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
  const level = Math.max(testLevel, voiceState === "connected" ? engineLevel : 0);

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
        {audio.inputMode === "ptt" ? (
          <div className="flex flex-col gap-1">
            <div className="flex items-center gap-2">
              <Button onClick={() => setCapturing(true)} className={cn("min-w-40", capturing && "border-accent text-accent")}>
                {capturing ? t("audio.pressKey") : audio.pttKey ? keyLabel(audio.pttKey) : t("audio.setKey")}
              </Button>
              {audio.pttKey && !capturing && (
                <Button variant="ghost" onClick={() => setAudio({ pttKey: null })}>
                  {t("audio.clearKey")}
                </Button>
              )}
            </div>
            <p className="text-xs text-subtle">
              {audio.pttKey ? t(onDesktop ? "audio.pttHelpGlobal" : "audio.pttHelp") : t("audio.pttNoKey")}
            </p>
          </div>
        ) : (
          <p className="text-xs text-subtle">{t("audio.vadHelp")}</p>
        )}
      </Section>

      {onDesktop && (
        <Section title={t("audio.globalShortcuts")}>
          <ShortcutField label={t("audio.muteShortcut")} value={desktop.muteShortcut} onChange={(v) => setDesktop({ muteShortcut: v })} />
          <ShortcutField label={t("audio.deafenShortcut")} value={desktop.deafenShortcut} onChange={(v) => setDesktop({ deafenShortcut: v })} />
          <p className="text-xs text-subtle">{t("audio.shortcutHint")}</p>
        </Section>
      )}

      <Section title={t("audio.test")}>
        <div className="flex items-center gap-3">
          <Button onClick={() => {
              setTestError(null);
              setTesting(!testing);
            }} variant={testing ? "primary" : "secondary"} className="shrink-0">
            {testing ? t("audio.testStop") : t("audio.testStart")}
          </Button>
          <LevelMeter level={level} label={t("audio.level")} />
        </div>
        {testError && (
          <p role="alert" className="text-sm text-danger">
            {testError}
          </p>
        )}
      </Section>

      {engine.capabilities.captureProcessing === false ? (
        <Section title={t("audio.processing")}>
          <p className="text-xs text-subtle">{t("audio.processingDesktop")}</p>
        </Section>
      ) : (
        <Section title={t("audio.processing")}>
          <Switch
            checked={audio.noiseSuppression}
            onCheckedChange={(v) => setAudio({ noiseSuppression: v })}
            label={t("audio.noiseSuppression")}
            description={t("audio.noiseSuppressionHint")}
          />
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
  const [nickname, setNickname] = useState(me?.nickname ?? "");
  const [away, setAway] = useState(me?.away ?? "");
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
      .catch(() => alive && useUi.getState().toast("error", tNow("settings.identityUnavailable")));
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
    const id = await controller.getIdentity();
    const blob = new Blob([JSON.stringify(id.exportBackup(), null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `voice-identity-${uid.slice(0, 8)}.json`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const importIdentity = async (f: File | undefined) => {
    if (!f) return;
    try {
      await controller.importIdentity(await f.text());
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
      await controller.updateProfile({ nickname: nickname.trim(), away: away.trim() });
      toast("success", t("common.saved"));
    } catch {
      toast("error", t("err.req.internal"));
    }
    setBusy(false);
  };

  return (
    <div>
      {online && (
        <Section title={t("identity.profile")}>
          <Field label={t("connect.nickname")}>
            {(id) => <Input id={id} value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={32} />}
          </Field>
          <Field label={t("identity.away")} hint={t("identity.awayHint")}>
            {(id) => <Input id={id} value={away} onChange={(e) => setAway(e.target.value)} maxLength={80} />}
          </Field>
          <div>
            <Button variant="primary" size="sm" busy={busy} onClick={() => void saveProfile()} disabled={!nickname.trim()}>
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
