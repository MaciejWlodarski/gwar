/**
 * Screen-sharing interface (docs/screen-share.md, "Interface"): the Share button, the preset
 * menu, the sharing bar, LIVE badges and the stage.
 */
import { Maximize, MessageSquare, MessageSquareOff, Minimize, MonitorOff, MonitorUp, PictureInPicture2, ScreenShareOff, X } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { countKey, useLanguage, useT, type Key } from "../i18n";
import { cn } from "../lib/cn";
import { isDesktop } from "../platform";
import { canActOn } from "../lib/permissions";
import type { Client } from "../proto/Client";
import type { ScreenShare } from "../proto/ScreenShare";
import { controller } from "../state/controller";
import { guestLinkPossible, useCanWatch, useScreenSupport } from "../state/screen-controller";
import { shareOf } from "../state/reducer";
import { slotOf, useScreen } from "../state/screen";
import { useSession } from "../state/stores";
import { type PublishStats } from "../voice/screen";
import { HEIGHTS, FRAME_RATES, type ScreenSpec } from "../voice/screen-quality";
import { useChannelPermission, useMe, useMemberName, useMyWho, useWhoOf } from "./hooks";
import { Button, Dialog, IconButton, panelCard, TalkingAvatar, Tooltip } from "./kit";

// -------------------------------------------------------------------- start

/** Why the Share button cannot start a share here, as its label; null when it can. */
function useShareRefusal(): string | null {
  const t = useT();
  const me = useMe();
  const kind = useSession(s => s.kind);
  const encrypted = useSession(s => (me?.channel != null ? !!s.channels[me.channel]?.encrypted : false));
  const allowed = useChannelPermission(me?.channel ?? null, "share_screen");
  const support = useScreenSupport();
  if (kind !== "gwar" || support === "hidden") return null;
  // Encrypted channels refuse until encrypted shares land (PR 7): never a share in the clear.
  if (encrypted) return t("screen.encryptedSoon");
  if (support === "macos") return t("screen.needsMacos");
  if (support === "unsupported") return t("screen.unsupported");
  if (support === "insecure") return t("screen.insecure");
  if (!allowed) return t("screen.noPermission");
  return null;
}

/**
 * The voice panel's Share button, beside mute and deafen. Hidden on TeamSpeak servers and in
 * the Linux desktop app (deferred); shown disabled with the reason elsewhere.
 */
export function ShareButton() {
  const t = useT();
  const own = useScreen(s => s.own);
  const kind = useSession(s => s.kind);
  const refusal = useShareRefusal();
  const support = useScreenSupport();
  if (kind !== "gwar" || support === "hidden") return null;
  if (own) {
    return (
      <IconButton label={t("screen.stop")} size="wide" active onClick={() => void controller.screen.stopShare()}
        className="bg-danger-soft text-danger hover:bg-danger hover:text-white">
        <MonitorOff className="size-[18px]" />
      </IconButton>
    );
  }
  return (
    <IconButton
      label={refusal ?? t("screen.share")}
      size="wide"
      aria-disabled={refusal ? true : undefined}
      // `getDisplayMedia` is called right here: nothing may be awaited before it.
      onClick={() => { if (!refusal) controller.screen.beginShare(); }}
      className={cn("bg-hover text-fg hover:bg-active", refusal && "cursor-not-allowed opacity-45 hover:bg-hover")}
    >
      <MonitorUp className="size-[18px]" />
    </IconButton>
  );
}

function QualityControls({ spec, bar = false }: { spec: ScreenSpec; bar?: boolean }) {
  const t = useT();
  const selectStyle = bar ? "h-7 rounded-md border border-white/30 bg-white/15 px-1.5 text-xs text-white [&>option]:text-black" : "h-8 rounded-md border border-line bg-side px-2 text-sm text-fg";
  return <div className="flex flex-wrap items-center gap-3">
    <fieldset>
      <legend className="mb-1 text-xs">{t("screen.mode")}</legend>
      <div role="radiogroup" aria-label={t("screen.mode")} className="flex gap-1">
        {(["smooth", "sharp"] as const).map(mode => <label key={mode} className={cn("flex cursor-pointer items-center gap-1.5 rounded-md border px-2 py-1 text-xs", bar ? "border-white/30" : spec.mode === mode ? "border-accent bg-accent-soft" : "border-line hover:bg-hover")}>
          <input type="radio" name={bar ? "sharing-mode" : "screen-mode"} className="accent-[var(--accent)]" checked={spec.mode === mode} onClick={() => { if (spec.mode === mode) controller.screen.setQuality({ mode }); }} onChange={() => controller.screen.setQuality({ mode })} />
          {t(`screen.mode.${mode}`)}
        </label>)}
      </div>
    </fieldset>
    <label className="flex flex-col gap-1 text-xs">{t("screen.resolution")}
      <select aria-label={t("screen.resolution")} value={spec.height} className={selectStyle} onChange={e => controller.screen.setQuality({ height: Number(e.target.value) as ScreenSpec["height"] })}>
        {HEIGHTS.map(height => <option key={height} value={height}>{height}p</option>)}
      </select>
    </label>
    <label className="flex flex-col gap-1 text-xs">{t("screen.frameRate")}
      <select aria-label={t("screen.frameRate")} value={spec.fps} className={selectStyle} onChange={e => controller.screen.setQuality({ fps: Number(e.target.value) as ScreenSpec["fps"] })}>
        {FRAME_RATES.map(fps => <option key={fps} value={fps}>{fps} fps</option>)}
      </select>
    </label>
  </div>;
}

/** The quality menu, after the browser's picker and before anything is published. */
export function SharePresetDialog() {
  const t = useT();
  const own = useScreen(s => s.own);
  const open = own?.phase === "choosing";
  const [busy, setBusy] = useState(false);
  const linkable = useSession(guestLinkPossible);
  if (!own || (own.phase !== "choosing" && !busy)) return null;
  return (
    <Dialog
      open={open}
      onOpenChange={next => { if (!next) void controller.screen.stopShare(); }}
      title={t("screen.presetTitle")}
      description={t(`screen.surface.${own.surface}` as Key)}
      footer={
        <>
          <Button variant="ghost" onClick={() => void controller.screen.stopShare()}>{t("common.cancel")}</Button>
          <Button variant="primary" busy={busy} onClick={() => { setBusy(true); void controller.screen.confirmShare().finally(() => setBusy(false)); }}>
            {t("screen.start")}
          </Button>
        </>
      }
    >
      <OwnPreview stream={own.stream} className="mb-3 aspect-video w-full rounded-md bg-black" />
      <QualityControls spec={own.spec} />
      <p className="mt-3 text-xs text-subtle">{t("screen.presetHint")}</p>
      {linkable && (
        // Shown before anything is published: no link exists until the share starts with it.
        <div className="mt-3 rounded-md border border-line bg-side px-3 py-2">
          <p className="text-xs text-muted">{t("screen.tsNotice")}</p>
          <label className="mt-1.5 flex cursor-pointer items-center gap-2 text-sm text-fg">
            <input type="checkbox" className="size-4 accent-[var(--accent)]" checked={own.guestLink} onChange={e => controller.screen.setGuestLink(e.target.checked)} />
            {t("screen.tsPost")}
          </label>
        </div>
      )}
    </Dialog>
  );
}

function OwnPreview({ stream, className }: { stream: MediaStream | null; className?: string }) {
  const ref = useRef<HTMLVideoElement>(null);
  useEffect(() => {
    const video = ref.current;
    if (!video) return;
    video.srcObject = stream;
    if (stream) void video.play().catch(() => {});
  }, [stream]);
  return <video ref={ref} muted playsInline autoPlay className={cn("object-contain", className)} />;
}

// ------------------------------------------------------------------ sharing

/** The red bar while I share: viewers, quality settings, an optional preview and Stop. */
export function SharingBar() {
  const t = useT();
  const lang = useLanguage();
  const own = useScreen(s => s.own);
  const links = useScreen(s => s.links);
  if (!own || (own.phase !== "starting" && own.phase !== "live")) return null;
  const watching = own.phase === "live" ? t(countKey(lang, "screen.watching", own.viewers), { count: own.viewers }) : t("screen.starting");
  const linked = !!own.share && !!links[own.share];
  return (
    <div role="status" aria-label={t("screen.sharingBar")} className="flex shrink-0 flex-wrap items-center gap-x-3 gap-y-1 bg-danger px-3 py-1.5 text-sm text-white">
      <MonitorUp aria-hidden className="size-4 shrink-0" />
      <span className="font-medium">{t("screen.sharingBar")}</span>
      <span data-testid="screen-viewers" className="text-white/85">{watching}</span>
      {own.phase === "live" && <SenderStats />}
      {linked && own.share && (
        <span data-testid="screen-ts-link" className="flex items-center gap-1.5 text-white/85">
          <span>{`${t("screen.tsLinkOn")} · ${t(countKey(lang, "screen.guests", own.guests), { count: own.guests })}`}</span>
          <Button size="sm" variant="secondary" title={t("screen.tsTurnOffTitle")} className="h-6 border-white/40 px-2 text-xs text-white hover:bg-white/15"
            onClick={() => void controller.screen.turnOffLink(own.share!)}>
            {t("screen.tsTurnOff")}
          </Button>
        </span>
      )}
      <span className="ml-auto flex items-center gap-2">
        <span className="text-xs">{`${own.spec.height}p${own.spec.fps} · ${t(`screen.mode.${own.spec.mode}`)}`}</span>
        <QualityControls spec={own.spec} bar />
        <Button size="sm" variant="secondary" aria-pressed={own.preview} className={cn("border-white/40 text-white hover:bg-white/15", own.preview && "bg-white/20")}
          onClick={() => controller.screen.setPreview(!own.preview)}>
          {t("screen.preview")}
        </Button>
        <Button size="sm" variant="secondary" className="border-white/40 text-white hover:bg-white/15" onClick={() => void controller.screen.stopShare()}>
          <ScreenShareOff className="size-4" /> {t("screen.stop")}
        </Button>
      </span>
      {own.preview && <OwnPreview stream={own.stream} className="fixed right-3 bottom-3 z-40 w-56 rounded-md border border-line-strong bg-black shadow-pop" />}
    </div>
  );
}

/**
 * What my encoder sends, and why it is held back: "cpu" means this computer cannot encode the
 * selected settings in time, "bandwidth" the connection. The encoder name tells software from hardware.
 */
function SenderStats() {
  const t = useT();
  const [stats, setStats] = useState<PublishStats | null>(null);
  useEffect(() => {
    let live = true;
    const poll = () => void controller.screen.ownStats().then(s => { if (live) setStats(s); });
    poll();
    const timer = setInterval(poll, 2000);
    return () => { live = false; clearInterval(timer); };
  }, []);
  if (!stats || !stats.height) return null;
  const mbps = (stats.bitrate / 1e6).toFixed(1);
  const encoder = stats.encoder
    ? t("screen.encoder", { name: stats.encoder, kind: t(stats.hardware === true ? "screen.encoderHardware" : stats.hardware === false ? "screen.encoderSoftware" : "screen.encoderUnknown") })
    : undefined;
  return (
    <span data-testid="screen-sender-stats" title={encoder} className="flex items-center gap-1.5 text-xs text-white/85 tabular-nums">
      {`${stats.height}p · ${stats.fps} fps · ${mbps} Mb/s${stats.codec ? ` · ${stats.codec}` : ""}`}
      {stats.limitation === "cpu" || stats.limitation === "bandwidth" ? (
        <span className="rounded bg-white/20 px-1.5 leading-5 font-medium text-white">
          {t(stats.limitation === "cpu" ? "screen.limitedCpu" : "screen.limitedBandwidth")}
        </span>
      ) : null}
    </span>
  );
}

// --------------------------------------------------------------------- live

/** "LIVE" next to a sharer in the channel tree; clicking it opens the share on the stage. */
export function LiveBadge({ client, name }: { client: Client; name: string }) {
  const t = useT();
  const share = useSession(s => shareOf(s, client.id));
  const isMe = useSession(s => s.me?.session === client.id);
  const watchable = useCanWatch();
  if (!client.sharing) return null;
  const pill = "shrink-0 rounded bg-danger px-1 text-[10px] leading-4 font-bold tracking-wide text-white";
  if (isMe) return <span className={pill} title={t("screen.liveSelf")}>{t("screen.live")}</span>;
  if (!watchable || !share) {
    return <span className={pill} title={t(!watchable && isDesktop() ? "screen.liveDesktop" : "screen.liveTitle", { name })}>{t("screen.live")}</span>;
  }
  return (
    <button type="button" aria-label={t("screen.liveTitle", { name })} title={t("screen.liveTitle", { name })}
      onClick={e => { e.stopPropagation(); void controller.screen.watch(share.share); }}
      onDoubleClick={e => e.stopPropagation()}
      className={cn(pill, "t cursor-pointer hover:brightness-110")}>
      {t("screen.live")}
    </button>
  );
}

// -------------------------------------------------------------------- stage

/** A watched share's video, from its slot's stream. */
function ShareVideo({ share, className, onDoubleClick, videoRef }: { share: string; className?: string; onDoubleClick?: () => void; videoRef?: (video: HTMLVideoElement | null) => void }) {
  const t = useT();
  const stream = useScreen(s => { const slot = slotOf(s, share); return slot === null ? null : (s.streams[slot] ?? null); });
  const grant = useScreen(s => s.watches.find(w => w.share === share && w.status === "watching"));
  const run = useScreen(s => s.transport.run);
  const ref = useRef<HTMLVideoElement | null>(null);
  const showing = !!stream && grant?.video?.ready === true;
  useEffect(() => {
    const video = ref.current;
    if (!video) return;
    if (video.srcObject !== stream) video.srcObject = stream;
    if (stream) void video.play().catch(() => {});
    const keyframe = grant?.video;
    if (!stream || !grant || grant.slot === null || !keyframe || showing) return;
    let active = true, timer: ReturnType<typeof setTimeout> | undefined, frame: number | undefined;
    const assignment = { run, id: grant.id, slot: grant.slot, share, generation: keyframe.generation };
    // Arm presentation and the static-share fallback only after this assignment's
    // server keyframe event. Neither cached dimensions nor an outgoing decode
    // can open a grant that has received no new media.
    if (video.requestVideoFrameCallback) frame = video.requestVideoFrameCallback(() => {
      if (active) useScreen.getState().dispatch({ type: "watchFrame", ...assignment, presented: true });
    });
    const check = async () => {
      const stats = await controller.screen.stats(share);
      if (!active) return;
      if (stats && stats.width > 0 && stats.height > 0) {
        useScreen.getState().dispatch({ type: "watchFrame", ...assignment, framesDecoded: stats.framesDecoded, fallback: Date.now() >= keyframe.at + 2_000 });
        if (useScreen.getState().watches.find(w => w.id === grant.id)?.video?.ready) return;
      }
      timer = setTimeout(() => { void check(); }, 250);
    };
    void check();
    return () => {
      active = false;
      if (timer !== undefined) clearTimeout(timer);
      if (frame !== undefined) video.cancelVideoFrameCallback(frame);
    };
  }, [stream, grant, run, share, showing]);
  return (
    <div className={cn("relative flex min-h-0 items-center justify-center overflow-hidden bg-black", className)}>
      <video
        ref={video => { ref.current = video; videoRef?.(video); }}
        data-share={share}
        muted
        autoPlay
        playsInline
        onDoubleClick={onDoubleClick}
        className={cn("size-full object-contain", !showing && "opacity-0")}
      />
      {!showing && (
        <div className="pointer-events-none absolute inset-0 flex items-center justify-center text-sm text-white/70">{t("screen.connecting")}</div>
      )}
    </div>
  );
}

function useSharer(share: ScreenShare | undefined) {
  const client = useSession(s => (share ? s.clients[share.client] : undefined));
  const name = useMemberName(client?.uid ?? "", client?.nickname ?? "?");
  return { client, name };
}

/** Moderators with `mute_members` there, ranked above the sharer, may stop a share. */
function useMayStop(share: ScreenShare | undefined, client: Client | undefined): boolean {
  const me = useMyWho();
  const target = useWhoOf(client?.uid ?? "", client?.groups ?? []);
  const groups = useSession(s => s.groups);
  const isMe = useSession(s => s.me?.session === client?.id);
  const mute = useChannelPermission(share?.channel ?? null, "mute_members");
  return !!share && !!client && !isMe && mute && canActOn(me, target, groups);
}

function QualityBadge({ share }: { share: string }) {
  const [label, setLabel] = useState("");
  useEffect(() => {
    let live = true;
    const poll = () => void controller.screen.stats(share).then(stats => {
      if (live) setLabel(stats && stats.height ? `${stats.height}p · ${stats.fps} fps` : "");
    });
    poll();
    const timer = setInterval(poll, 2000);
    return () => { live = false; clearInterval(timer); };
  }, [share]);
  if (!label) return null;
  return <span data-testid="screen-quality" className="rounded bg-white/10 px-1.5 text-[11px] leading-5 text-white/80 tabular-nums">{label}</span>;
}

const stageButton = "text-white/80 hover:bg-white/10 hover:text-white";

/**
 * The stage: the share in focus large, other shares of its channel and every other watched
 * share as cards (click to swap), with the chat beside it unless hidden.
 */
export function Stage({ card }: { card?: boolean }) {
  const t = useT();
  const stage = useScreen(s => s.stage);
  const watches = useScreen(s => s.watches);
  const chatHidden = useScreen(s => s.chatHidden);
  const shares = useSession(s => s.shares);
  const me = useSession(s => s.me?.session);
  const info = stage ? shares[stage] : undefined;
  const { client, name } = useSharer(info);
  const mayStop = useMayStop(info, client);
  const linked = useScreen(s => (stage ? !!s.links[stage] : false));
  const frame = useRef<HTMLDivElement>(null);
  const video = useRef<HTMLVideoElement | null>(null);
  const [fullscreen, setFullscreen] = useState(false);
  const pip = typeof document !== "undefined" && document.pictureInPictureEnabled === true;

  useEffect(() => {
    const changed = () => setFullscreen(document.fullscreenElement === frame.current && !!frame.current);
    document.addEventListener("fullscreenchange", changed);
    return () => document.removeEventListener("fullscreenchange", changed);
  }, []);

  const toggleFullscreen = () => {
    if (document.fullscreenElement) void document.exitFullscreen().catch(() => {});
    else void frame.current?.requestFullscreen?.().catch(() => {});
  };

  // F toggles fullscreen, unless typing.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (e.key.toLowerCase() !== "f" || e.ctrlKey || e.metaKey || e.altKey || target?.closest("input, textarea, select, [contenteditable=true]")) return;
      e.preventDefault();
      toggleFullscreen();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (!stage) return null;
  const watched = new Set(watches.map(w => w.share));
  const cards = [
    ...watches.map(w => w.share).filter(s => s !== stage),
    ...Object.values(shares).filter(s => info && s.channel === info.channel && s.share !== stage && s.client !== me && !watched.has(s.share)).map(s => s.share),
  ];

  return (
    <section aria-label={t("screen.stage")} className={cn("flex min-w-0 flex-1 flex-col bg-[#0b0b0d] text-white", card && panelCard)}>
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-white/10 px-3">
        {client && <TalkingAvatar name={name} seed={client.uid} size={24} talking={client.talking} />}
        <h2 className="min-w-0 truncate text-[15px] font-semibold">{t("screen.sharerTitle", { name })}</h2>
        <span className="shrink-0 rounded bg-danger px-1 text-[10px] leading-4 font-bold tracking-wide">{t("screen.live")}</span>
        <QualityBadge share={stage} />
        <span className="ml-auto flex items-center gap-1">
          {mayStop && linked && (
            <Button size="sm" variant="secondary" className="border-white/30 text-white hover:bg-white/10" onClick={() => void controller.screen.turnOffLink(stage)}>
              {t("screen.tsTurnOffOther")}
            </Button>
          )}
          {mayStop && (
            <Button size="sm" variant="secondary" className="border-white/30 text-white hover:bg-white/10" onClick={() => void controller.screen.stopOther(stage)}>
              <ScreenShareOff className="size-4" /> {t("screen.stopOther")}
            </Button>
          )}
          {pip && (
            <IconButton label={t("screen.pip")} className={stageButton} onClick={() => void video.current?.requestPictureInPicture().catch(() => {})}>
              <PictureInPicture2 className="size-[18px]" />
            </IconButton>
          )}
          <IconButton label={t(fullscreen ? "screen.exitFullscreen" : "screen.fullscreen")} shortcut="F" className={stageButton} onClick={toggleFullscreen}>
            {fullscreen ? <Minimize className="size-[18px]" /> : <Maximize className="size-[18px]" />}
          </IconButton>
          <IconButton label={t(chatHidden ? "screen.showChat" : "screen.hideChat")} className={cn(stageButton, "max-md:hidden")}
            onClick={() => useScreen.getState().dispatch({ type: "chat", hidden: !chatHidden })}>
            {chatHidden ? <MessageSquare className="size-[18px]" /> : <MessageSquareOff className="size-[18px]" />}
          </IconButton>
          <Button size="sm" variant="secondary" className="border-white/30 text-white hover:bg-white/10" onClick={() => void controller.screen.unwatch(stage)}>
            <X className="size-4" /> {t("screen.stopWatching")}
          </Button>
        </span>
      </header>
      <div ref={frame} className="flex min-h-0 flex-1 flex-col bg-black">
        <ShareVideo key={stage} share={stage} className="flex-1" onDoubleClick={toggleFullscreen} videoRef={v => { video.current = v; }} />
      </div>
      {cards.length > 0 && (
        <div className="flex shrink-0 gap-2 overflow-x-auto border-t border-white/10 p-2">
          {cards.map(share => <ShareCard key={share} share={share} watched={watched.has(share)} />)}
        </div>
      )}
    </section>
  );
}

function ShareCard({ share, watched }: { share: string; watched: boolean }) {
  const t = useT();
  const info = useSession(s => s.shares[share]);
  const { client, name } = useSharer(info);
  const open = () => (watched ? controller.screen.focus(share) : void controller.screen.watch(share));
  const onKey = (e: ReactKeyboardEvent) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); open(); } };
  let body: ReactNode;
  if (watched) body = <ShareVideo share={share} className="aspect-video w-full" />;
  else body = (
    <div className="flex aspect-video w-full items-center justify-center bg-white/5">
      <span className="rounded-md bg-white/10 px-2 py-1 text-xs font-medium">{t("screen.watch")}</span>
    </div>
  );
  return (
    <Tooltip label={t("screen.sharerTitle", { name })}>
      <div role="button" tabIndex={0} aria-label={watched ? t("screen.sharerTitle", { name }) : `${t("screen.watch")}: ${t("screen.sharerTitle", { name })}`}
        onClick={open} onKeyDown={onKey}
        className="t w-44 shrink-0 cursor-pointer overflow-hidden rounded-md border border-white/10 hover:border-white/40">
        {body}
        <div className="flex items-center gap-1.5 px-1.5 py-1 text-xs">
          {client && <TalkingAvatar name={name} seed={client.uid} size={16} talking={client.talking} />}
          <span className="min-w-0 truncate">{name}</span>
        </div>
      </div>
    </Tooltip>
  );
}
