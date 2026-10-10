import { Headphones, HeadphoneOff, Mic, MicOff, PhoneOff, Settings, Wifi } from "lucide-react";
import { useT } from "../i18n";
import { cn } from "../lib/cn";
import { controller, describeVoiceError } from "../state/controller";
import { canOpenMicrophoneSettings, openMicrophoneSettings } from "../platform";
import { useSettings } from "../state/settings";
import { useSession, useUi, useVoice } from "../state/stores";
import { useMe, keyLabel, modKey } from "./hooks";
import { Avatar, IconButton, Button } from "./kit";
import { ConnectBadge } from "./badges";

/** Mic toggle that doubles as a level meter and talking indicator. */
export function MicButton({ size = "md" }: { size?: "sm" | "md" }) {
  const t = useT();
  const muted = useVoice((s) => s.muted);
  const deafened = useVoice((s) => s.deafened);
  const level = useVoice((s) => s.level);
  const micError = useVoice((s) => s.micError);
  const pttActive = useVoice((s) => s.pttActive);
  const mode = useSettings((s) => s.audio.inputMode);
  const pttKey = useSettings((s) => s.audio.pttKey);
  const me = useMe();
  const off = muted || deafened || !!micError;
  const talking = !!me?.talking && !off;
  const ptt = mode === "ptt";
  const label = micError ? describeVoiceError(micError) : off ? t("voice.unmute") : t("voice.mute");

  return (
    <IconButton
      label={ptt && !off ? t("voice.pttHint", { key: pttKey ? keyLabel(pttKey) : "—" }) : label}
      shortcut={`${modKey}+⇧+M`}
      active={off}
      onClick={() => (micError ? void controller.retryMic() : controller.toggleMute())}
      className={cn(
        "relative overflow-hidden",
        size === "md" ? "size-9" : "size-8",
        off ? "bg-danger-soft text-danger hover:bg-danger hover:text-white" : "bg-hover text-fg hover:bg-active",
        talking && "ring-2 ring-ok",
        ptt && pttActive && !off && "ring-2 ring-ok",
      )}
    >
      {!off && (
        <span
          aria-hidden
          className="absolute inset-x-0 bottom-0 bg-ok/30 transition-[height] duration-75"
          style={{ height: `${Math.round(level * 100)}%` }}
        />
      )}
      {off ? <MicOff className="relative size-[18px]" /> : <Mic className="relative size-[18px]" />}
    </IconButton>
  );
}

export function DeafenButton({ size = "md" }: { size?: "sm" | "md" }) {
  const t = useT();
  const deafened = useVoice((s) => s.deafened);
  return (
    <IconButton
      label={deafened ? t("voice.undeafen") : t("voice.deafen")}
      shortcut={`${modKey}+⇧+D`}
      active={deafened}
      onClick={() => controller.toggleDeafen()}
      className={cn(
        size === "md" ? "size-9" : "size-8",
        deafened ? "bg-danger-soft text-danger hover:bg-danger hover:text-white" : "bg-hover text-fg hover:bg-active",
      )}
    >
      {deafened ? <HeadphoneOff className="size-[18px]" /> : <Headphones className="size-[18px]" />}
    </IconButton>
  );
}

/** My nickname, with the Connect badge once the server has confirmed my handle. */
function SelfName({ nickname }: { nickname?: string }) {
  const handle = useSession((s) => (s.me ? s.members[s.me.uid]?.connect : undefined));
  return (
    <div className="flex min-w-0 items-center gap-1">
      <span className="truncate text-sm font-medium">{nickname}</span>
      <ConnectBadge handle={handle} />
    </div>
  );
}

export function VoicePanel() {
  const t = useT();
  const voiceState = useVoice((s) => s.state);
  const micError = useVoice((s) => s.micError);
  const me = useMe();
  const channelName = useSession((s) => (me?.channel != null ? s.channels[me.channel]?.name : undefined));
  const phase = useSession((s) => s.phase);
  // TeamSpeak servers always keep you in a channel: there is nothing to leave.
  const canLeave = useSession((s) => s.kind === "vc");
  const openDialog = useUi((s) => s.openDialog);

  if (me && me.channel === null) {
    return (
      <div className="border-t border-line bg-side">
        <div className="flex items-center gap-1 px-3 py-2.5">
          <Avatar name={me.nickname} seed={me.uid} size={32} />
          <div className="mr-auto ml-1.5 min-w-0 leading-tight">
            <SelfName nickname={me.nickname} />
            <div className="flex items-center gap-1 truncate text-xs text-subtle" title={t("voice.notInHint")}>
              <HeadphoneOff aria-hidden className="size-3 shrink-0" />
              <span className="truncate">{t("voice.notIn")}</span>
            </div>
          </div>
          <IconButton label={t("settings.title")} onClick={() => openDialog({ kind: "settings", tab: "audio" })}>
            <Settings className="size-[18px]" />
          </IconButton>
        </div>
      </div>
    );
  }

  const status =
    phase === "reconnecting"
      ? { tone: "warn", text: t("voice.reconnecting") }
      : voiceState === "connected"
        ? { tone: "ok", text: t("voice.connected") }
        : voiceState === "failed"
          ? { tone: "danger", text: t("voice.failed") }
          : { tone: "warn", text: t("voice.connecting") };

  return (
    <div className="border-t border-line bg-side">
      <div className="flex items-center gap-2 px-3 pt-2.5">
        <Wifi
          className={cn(
            "size-4 shrink-0",
            status.tone === "ok" && "text-ok",
            status.tone === "warn" && "animate-pulse text-warn",
            status.tone === "danger" && "text-danger",
          )}
        />
        <div className="min-w-0 flex-1 leading-tight">
          <div
            className={cn(
              "text-[13px] font-medium",
              status.tone === "ok" && "text-ok",
              status.tone === "warn" && "text-warn",
              status.tone === "danger" && "text-danger",
            )}
          >
            {status.text}
          </div>
          {channelName && <div className="truncate text-xs text-muted">{channelName}</div>}
        </div>
        {voiceState === "failed" && phase === "online" && (
          <Button size="sm" variant="secondary" onClick={() => controller.retryVoice()}>
            {t("voice.retry")}
          </Button>
        )}
      </div>
      {micError && (
        <div role="alert" className="mx-3 mt-2 rounded-md bg-danger-soft px-2 py-1.5 text-xs text-danger">
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
      <div className="flex items-center gap-1 px-3 py-2.5">
        {me && <Avatar name={me.nickname} seed={me.uid} size={32} className={cn(me.talking && "talking-ring")} />}
        <div className="mr-auto ml-1.5 min-w-0 leading-tight">
          <SelfName nickname={me?.nickname} />
          <div className="truncate text-xs text-subtle">{me?.away ?? ""}</div>
        </div>
        <MicButton />
        <DeafenButton />
        <IconButton label={t("settings.title")} onClick={() => openDialog({ kind: "settings", tab: "audio" })}>
          <Settings className="size-[18px]" />
        </IconButton>
        {canLeave && (
          <IconButton label={t("voice.leave")} tone="danger" onClick={() => void controller.leaveVoice()}>
            <PhoneOff className="size-[18px]" />
          </IconButton>
        )}
      </div>
    </div>
  );
}
