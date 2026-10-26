import { Headphones, HeadphoneOff, Mic, MicOff, PhoneOff, SignalHigh, SignalLow, SignalMedium } from "lucide-react";
import type { ReactNode } from "react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller, describeVoiceError } from "../state/controller";
import { canOpenMicrophoneSettings, openMicrophoneSettings } from "../platform";
import { useSettings } from "../state/settings";
import { useSession, useVoice } from "../state/stores";
import { useChannelPermission, useMe, keyLabel, modKey } from "./hooks";
import { Button, IconButton, TalkingAvatar } from "./kit";
import { ConnectBadge } from "./badges";
import { ShareButton } from "./Screen";

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

/** The round trip to the server, as signal bars and ms; nothing until the first measurement. */
function RoundTrip() {
  const t = useT();
  const rtt = useSession((s) => s.rtt);
  if (rtt === null) return null;
  const [Icon, tone] = rtt < 100 ? [SignalHigh, "text-ok"] : rtt < 200 ? [SignalMedium, "text-warn"] : [SignalLow, "text-danger"];
  return (
    <span className={cn("ml-auto flex shrink-0 items-center gap-0.5 tabular-nums", tone)} title={t("voice.rtt", { ms: String(rtt) })}>
      <Icon aria-hidden className="size-3.5" />
      {rtt < 1 ? "<1" : rtt} ms
    </span>
  );
}

export function VoicePanel() {
  const t = useT();
  const voiceState = useVoice((s) => s.state);
  const encryption = useVoice((s) => s.encryption);
  const micError = useVoice((s) => s.micError);
  const me = useMe();
  // Encrypted channels never show an alarm, even when the engine refused them.
  const encryptedChannel = useSession((s) => (me?.channel != null ? !!s.channels[me.channel]?.encrypted : false));
  const channelName = useSession((s) => (me?.channel != null ? s.channels[me.channel]?.name : undefined));
  const phase = useSession((s) => s.phase);
  // TeamSpeak servers always keep you in a channel: there is nothing to leave.
  const canLeave = useSession((s) => s.kind === "gwar");
  const canSpeak = useChannelPermission(me?.channel ?? null, "speak");
  const member = useSession((s) => (s.me ? s.members[s.me.uid] : undefined));

  if (me && me.channel === null) {
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

  // Encrypted calls: "Securing voice…" until a sending key is active, in the
  // muted style; a stopped engine is "Voice unavailable" with Retry, no alarm.
  const status =
    phase === "reconnecting"
      ? { tone: "warn", text: t("voice.reconnecting") }
      : encryption === "failed"
        ? { tone: "muted", text: t("voice.failed") }
        : encryption === "securing" && voiceState !== "failed"
          ? { tone: "muted", text: t("voice.securing") }
          : voiceState === "connected"
            ? { tone: "ok", text: t("voice.connected") }
            : voiceState === "failed"
              ? { tone: encryption || encryptedChannel ? "muted" : "danger", text: t("voice.failed") }
              : { tone: "warn", text: t("voice.connecting") };
  const unavailable = voiceState === "failed" || encryption === "failed";
  // Nobody hears me: a moderator muted or deafened me, or this channel does not let me speak.
  const silencedNote = me?.server_deafened
    ? t("voice.serverDeafened")
    : me?.server_muted
      ? t("voice.serverMuted")
      : !canSpeak
        ? t("voice.cannotSpeak")
        : null;
  const tone = cn(
    status.tone === "ok" && "text-ok",
    status.tone === "warn" && "text-warn",
    status.tone === "danger" && "text-danger",
    status.tone === "muted" && "text-muted",
  );

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
          <div className="flex min-w-0 items-center gap-1.5 text-xs" title={status.text}>
            <span aria-hidden className={cn("size-2 shrink-0 rounded-full bg-current", tone, status.tone === "warn" && "animate-pulse")} />
            {status.tone === "ok" && channelName ? (
              <>
                <span className="sr-only">{status.text}</span>
                <span className="truncate text-muted">{channelName}</span>
              </>
            ) : (
              <span className={cn("truncate", tone)}>{status.text}</span>
            )}
            {status.tone === "ok" && <RoundTrip />}
          </div>
        </div>
        {unavailable && phase === "online" && (
          <Button size="sm" variant="secondary" onClick={() => controller.retryVoice()}>
            {t("voice.retry")}
          </Button>
        )}
        {canLeave && (
          <IconButton label={t("voice.leave")} tone="danger" onClick={() => void controller.leaveVoice()}>
            <PhoneOff className="size-4" />
          </IconButton>
        )}
      </div>
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
      </div>
    </div>
  );
}
