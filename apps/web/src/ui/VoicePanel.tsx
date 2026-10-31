import { Headphones, HeadphoneOff, Mic, MicOff, MonitorUp, PhoneOff, SlidersHorizontal } from "lucide-react";
import { useState, type ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller, describeVoiceError } from "../state/controller";
import { canOpenMicrophoneSettings, openMicrophoneSettings } from "../platform";
import { useScreen } from "../state/screen";
import { useSettings } from "../state/settings";
import { myChannelId } from "../state/reducer";
import { useSession, useUi, useVoice } from "../state/stores";
import { qualityTone, useChannelPermission, useConnectionQuality, useMe, keyLabel, modKey } from "./hooks";
import { Button, Dialog, IconButton, TalkingAvatar, Tooltip } from "./kit";
import { showVoiceChannel, useVoicePill } from "./nav";
import { ConnectBadge } from "./badges";
import { ShareButton } from "./Screen";
import { AudioDevices } from "./AudioDevices";
import { ConnectionIndicator } from "./ConnectionQuality";

/** Mic toggle that doubles as a level meter and talking indicator. */
export function MicButton({ size = "lg" }: { size?: "md" | "lg" | "wide" }) {
  const t = useT();
  const muted = useVoice((s) => s.muted);
  const deafened = useVoice((s) => s.deafened);
  const level = useVoice((s) => s.level);
  const micError = useVoice((s) => s.micError);
  const pttActive = useVoice((s) => s.pttActive);
  const mode = useSettings((s) => s.audio.inputMode);
  const pttKey = useSettings((s) => s.audio.pttKey);
  const off = muted || deafened || !!micError;
  const ptt = mode === "ptt";
  const label = micError ? describeVoiceError(micError) : off ? t("voice.unmute") : t("voice.mute");

  return (
    <IconButton
      label={ptt && !off ? t("voice.pttHint", { key: pttKey ? keyLabel(pttKey) : "—" }) : label}
      shortcut={`${modKey}+⇧+M`}
      active={off}
      size={size}
      onClick={() => (micError ? void controller.retryMic() : controller.toggleMute())}
      className={cn(
        "relative overflow-hidden",
        off ? "bg-danger-soft text-danger hover:bg-danger hover:text-white" : "bg-hover text-fg hover:bg-active",
        ptt && pttActive && !off && "ring-2 ring-talk",
      )}
    >
      {!off && (
        <span
          aria-hidden
          className="absolute inset-x-0 bottom-0 bg-talk/30 transition-[height] duration-75"
          style={{ height: `${Math.round(level * 100)}%` }}
        />
      )}
      {off ? <MicOff className="relative size-[18px]" /> : <Mic className="relative size-[18px]" />}
    </IconButton>
  );
}

export function DeafenButton({ size = "lg" }: { size?: "md" | "lg" | "wide" }) {
  const t = useT();
  const deafened = useVoice((s) => s.deafened);
  return (
    <IconButton
      label={deafened ? t("voice.undeafen") : t("voice.deafen")}
      shortcut={`${modKey}+⇧+D`}
      active={deafened}
      size={size}
      onClick={() => controller.toggleDeafen()}
      className={deafened ? "bg-danger-soft text-danger hover:bg-danger hover:text-white" : "bg-hover text-fg hover:bg-active"}
    >
      {deafened ? <HeadphoneOff className="size-[18px]" /> : <Headphones className="size-[18px]" />}
    </IconButton>
  );
}

/** Always visible, including on touch screens. */
export function AudioDeviceButton({ size = "wide" }: { size?: "wide" | "md" }) {
  const t = useT();
  const [open, setOpen] = useState(false);
  return <>
    <IconButton size={size} label={t("audio.devices")} onClick={() => setOpen(true)} className="bg-hover text-fg hover:bg-active"><SlidersHorizontal className="size-[18px]" /></IconButton>
    <Dialog open={open} onOpenChange={setOpen} title={t("audio.devices")} width="max-w-md" footer={<Button onClick={() => { setOpen(false); useUi.getState().openSettings("audio"); }}>{t("person.voiceSettings")}</Button>}>
      {open && <AudioDevices compact />}
    </Dialog>
  </>;
}

/** Who I am here: avatar (with the talking ring), nickname with the Connect badge, and one line under it. */
function SelfRow({ sub, subTitle }: { sub: ReactNode; subTitle?: string }) {
  const me = useMe();
  const member = useSession((s) => (s.me ? s.members[s.me.uid] : undefined));
  if (!me) return null;
  return (
    <div className="flex min-w-0 items-center gap-2 px-1">
      <TalkingAvatar name={me.nickname} seed={me.uid} uid={me.uid} size={32} talking={me.talking && me.channel !== null} />
      <div className="min-w-0 flex-1 leading-tight">
        <div className="flex min-w-0 items-center gap-1">
          <span className="truncate text-sm font-semibold">{me.nickname}</span>
          <ConnectBadge member={member} name={me.nickname} />
        </div>
        {sub && (
          <div className="flex min-w-0 items-center gap-1 text-xs text-subtle" title={subTitle}>
            {sub}
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * How my voice connection is doing, in words and a tone (the panel's and the pill's status line).
 * Connected, the tone is the connection quality, the way Discord colours "Voice Connected".
 */
function useVoiceStatus() {
  const t = useT();
  const voiceState = useVoice((s) => s.state);
  const encryption = useVoice((s) => s.encryption);
  const reconnecting = useVoice((s) => s.reconnecting);
  const phase = useSession((s) => s.phase);
  const failure = useVoice((s) => s.failure);
  const status: { tone: "ok" | "warn" | "danger" | "muted"; text: string } =
    failure
      ? { tone: "danger", text: t("voice.failed") }
      : phase === "reconnecting" || reconnecting
        ? { tone: "warn", text: t("voice.reconnecting") }
        : encryption === "securing" && voiceState !== "failed"
          ? { tone: "muted", text: t("voice.securing") }
          : voiceState === "idle" && phase === "online"
            ? { tone: "muted", text: t("voice.notIn") }
            : voiceState === "connected"
              ? { tone: "ok", text: t("voice.connected") }
              : { tone: "warn", text: t("voice.connecting") };
  const quality = useConnectionQuality()?.quality;
  const tone = cn(
    status.tone === "ok" && qualityTone[quality ?? "good"],
    status.tone === "warn" && "text-warn",
    status.tone === "danger" && "text-danger",
    status.tone === "muted" && "text-muted",
  );

  return { status, tone };
}

/** The way out of voice. TeamSpeak servers always keep you in a channel: there is nothing to leave. */
function LeaveButton() {
  const t = useT();
  const canLeave = useSession((s) => s.kind === "gwar");
  if (!canLeave) return null;
  return (
    <IconButton label={t("voice.leave")} tone="danger" onClick={() => void controller.leaveVoice()}>
      <PhoneOff className="size-4" />
    </IconButton>
  );
}

export function VoicePanel() {
  const t = useT();
  const [showReason, setShowReason] = useState(false);
  const retrying = useVoice((s) => s.reconnecting);
  const failure = useVoice((s) => s.failure);
  const voiceState = useVoice((s) => s.state);
  const encryption = useVoice((s) => s.encryption);
  const micError = useVoice((s) => s.micError);
  const me = useMe();
  const channelName = useSession((s) => (me?.channel != null ? s.channels[me.channel]?.name : undefined));
  const phase = useSession((s) => s.phase);
  const canSpeak = useChannelPermission(me?.channel ?? null, "speak");
  const member = useSession((s) => (s.me ? s.members[s.me.uid] : undefined));
  const { status, tone } = useVoiceStatus();
  const recovering = !failure && (retrying || phase === "reconnecting");

  if (me && me.channel === null && !failure && !recovering) {
    return (
      <div className="m-2 mt-1 rounded-xl bg-raised p-2">
        <SelfRow
          subTitle={t("voice.notInHint")}
          sub={
            <>
              <HeadphoneOff aria-hidden className="size-3 shrink-0" />
              <span className="truncate">{t("voice.notIn")}</span>
            </>
          }
        />
      </div>
    );
  }

  const unavailable = voiceState === "failed" || encryption === "failed" || voiceState === "idle";
  // Nobody hears me: a moderator muted or deafened me, or this channel does not let me speak.
  const silencedNote = me?.server_deafened
    ? t("voice.serverDeafened")
    : me?.server_muted
      ? t("voice.serverMuted")
      : !canSpeak
        ? t("voice.cannotSpeak")
        : null;
  return (
    <div className="m-2 mt-1 flex flex-col gap-2 rounded-xl bg-raised p-2">
      {/* Who I am, where, and how well: one row, the way out at its end. */}
      <div className="flex min-w-0 items-center gap-2 pl-1">
        {me && <TalkingAvatar name={me.nickname} seed={me.uid} uid={me.uid} size={32} talking={me.talking} />}
        <div className="min-w-0 flex-1 leading-tight" title={me?.away ?? undefined}>
          <div className="flex min-w-0 items-center gap-1">
            <span className="truncate text-sm font-semibold">{me?.nickname}</span>
            {me && <ConnectBadge member={member} name={me.nickname} />}
          </div>
          {!failure && !recovering && <div data-testid="voice-status" className="flex min-w-0 items-center gap-1.5 text-xs" title={status.text}>
            <span aria-hidden className={cn("size-2 shrink-0 rounded-full bg-current", tone, status.tone === "warn" && "animate-pulse")} />
            {status.tone === "ok" && channelName ? (
              <>
                <span className="sr-only">{status.text}</span>
                <span className="truncate text-muted">{channelName}</span>
              </>
            ) : (
              <span className={cn("truncate", tone)}>{status.text}</span>
            )}
            {status.tone === "ok" && <ConnectionIndicator variant="panel" />}
          </div>}
        </div>
        {unavailable && !failure && !recovering && phase === "online" && (
          <Button size="sm" variant="secondary" onClick={() => controller.retryVoice()}>
            {t(voiceState === "idle" ? "tree.joinVoice" : "voice.retry")}
          </Button>
        )}
        {me?.channel != null && <LeaveButton />}
      </div>
      {(failure || recovering) && <div data-testid="voice-status" className="flex items-center gap-1.5 px-1 text-xs">
        <span aria-hidden className={cn("size-2 shrink-0 rounded-full bg-current", tone)} />
        <span className={tone}>{status.text}</span>
        <div className="ml-auto flex shrink-0 items-center gap-1.5">
          {failure && <button onClick={() => setShowReason(true)} className="t cursor-pointer text-muted underline underline-offset-2 hover:text-fg">{t("voice.why")}</button>}
          {phase === "online" && <>
            {failure && <span aria-hidden className="text-subtle">·</span>}
            <button onClick={() => controller.retryVoice()} className={cn("t cursor-pointer hover:underline", failure ? "font-medium text-fg" : "text-muted hover:text-fg")}>{t("voice.retry")}</button>
          </>}
        </div>
      </div>}
      <Dialog open={showReason && !!failure} onOpenChange={setShowReason} title={t("voice.failed")} description={failure ?? undefined}>{null}</Dialog>
      {micError && (
        <div role="alert" className="rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
          {describeVoiceError(micError)}
          {micError.kind === "mic_denied" && canOpenMicrophoneSettings() && (
            <button
              className="ml-1 cursor-pointer font-medium underline underline-offset-2"
              onClick={() => void openMicrophoneSettings().catch((e: unknown) => console.warn("[desktop] privacy settings:", e))}
            >
              {t("voice.openPrivacySettings")}
            </button>
          )}
          <button className="ml-1 cursor-pointer font-medium underline underline-offset-2" onClick={() => void controller.retryMic()}>
            {t("voice.retryMic")}
          </button>
        </div>
      )}
      {silencedNote && (
        <div role="status" className="flex items-center gap-1.5 rounded-md bg-warn/10 px-2 py-1.5 text-xs text-warn">
          <MicOff aria-hidden className="size-3.5 shrink-0" />
          <span className="min-w-0">{silencedNote}</span>
        </div>
      )}
      <div className="grid auto-cols-fr grid-flow-col gap-1.5">
        <MicButton size="wide" />
        <DeafenButton size="wide" />
        <ShareButton />
        <AudioDeviceButton />
      </div>
    </div>
  );
}

/**
 * The voice panel in brief, floating at the bottom of the main card wherever the server view (and its
 * panel) is not shown: how voice is doing, where I am (a way back there), and mic, deafen and leave.
 */
export function VoicePill() {
  const t = useT();
  const shown = useVoicePill();
  const { status, tone } = useVoiceStatus();
  const channelName = useSession((s) => {
    const channel = myChannelId(s);
    return channel === null ? undefined : s.channels[channel]?.name;
  });
  const serverName = useSession((s) => s.server?.name);
  const sharing = useScreen((s) => s.own?.phase === "live");
  if (!shown) return null;
  return (
    <section
      aria-label={t("voice.pill")}
      className="absolute bottom-3 left-1/2 z-10 flex w-max max-w-[calc(100%-1.5rem)] -translate-x-1/2 items-center gap-2 rounded-[14px] border border-line-strong bg-raised p-1.5 pl-2 shadow-pop"
    >
      <Tooltip label={t("voice.pillOpen")}>
        <button onClick={showVoiceChannel} className="t min-w-0 cursor-pointer rounded-md px-1.5 py-0.5 text-left leading-tight hover:bg-hover md:max-w-64">
          <span className="flex min-w-0 items-center gap-1.5 text-xs font-medium">
            <span aria-hidden className={cn("size-2 shrink-0 rounded-full bg-current", tone, status.tone === "warn" && "animate-pulse")} />
            <span className={cn("truncate", tone)}>{status.text}</span>
            {sharing && (
              <span className="flex shrink-0 items-center text-accent" title={t("screen.liveSelf")}>
                <MonitorUp aria-hidden className="size-3.5" />
                <span className="sr-only">{t("screen.liveSelf")}</span>
              </span>
            )}
          </span>
          <span className="block truncate text-xs text-muted">
            #{channelName} · {serverName}
          </span>
        </button>
      </Tooltip>
      <div className="flex shrink-0 items-center gap-1">
        {status.tone === "ok" && <ConnectionIndicator variant="pill" />}
        <MicButton size="md" />
        <DeafenButton size="md" />
        <LeaveButton />
      </div>
    </section>
  );
}
