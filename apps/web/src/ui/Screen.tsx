/**
 * Screen-sharing interface (docs/screen-share.md, "Interface"): the Share button, the preset
 * menu, the sharing bar, LIVE badges and the stage.
 */
import { Maximize, MessageSquare, MessageSquareOff, Minimize, MonitorOff, MonitorUp, PictureInPicture2, RotateCw, ScreenShareOff, X } from "lucide-react";
import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type ReactNode } from "react";
import { countKey, useLanguage, useT, type Key } from "../i18n";
import { cn } from "../lib/cn";
import { canOpenScreenRecordingSettings, isDesktop, openScreenRecordingSettings } from "../platform";
import { captureApp, captureOs } from "../voice/screen-permission";
import { canActOn } from "../lib/permissions";
import type { Client } from "../proto/Client";
import type { ScreenPauseReason } from "../proto/ScreenPauseReason";
import type { ScreenShare } from "../proto/ScreenShare";
import { controller } from "../state/controller";
import { guestLinkPossible, useCanWatch, useScreenSupport } from "../state/screen-controller";
import { myChannelId, shareOf } from "../state/reducer";
import { slotOf, useScreen } from "../state/screen";
import { useSession, useUi } from "../state/stores";
import { type PublishStats } from "../voice/screen";
import { HEIGHTS, FRAME_RATES, type ScreenSpec } from "../voice/screen-quality";
import { useChannelPermission, useMe, useMemberName, useMyWho, useWhoOf } from "./hooks";
import { BackButton, Button, Dialog, IconButton, panelCard, TalkingAvatar, Tooltip } from "./kit";
import { paneBack } from "./nav";

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

/** A stopped share or OS refusal needs an action that does not expire. */
export function ScreenNotice() {
  const notice = useScreen(s => s.notice);
  return notice ? <ScreenNoticeContent key={notice} notice={notice} /> : null;
}

function ScreenNoticeContent({ notice }: { notice: "reconnect" | "permission" | "permissionHint" }) {
  const t = useT();
  const [help, setHelp] = useState(false);
  const own = useScreen(s => s.own);
  const phase = useSession(s => s.phase);
  const channel = useSession(myChannelId);
  const title = notice === "permissionHint" ? "screen.permissionHintTitle" : "screen.permissionTitle";
  return (
    <>
      <div role={notice === "permissionHint" ? "status" : "alert"} data-testid="screen-notice" className="flex shrink-0 items-center gap-2 border-b border-line-strong bg-surface px-2 py-1.5 text-xs text-fg sm:px-4 md:text-sm">
        <MonitorOff aria-hidden className={cn("hidden size-4 shrink-0 sm:block", notice === "permissionHint" ? "text-muted" : "text-danger")} />
        <p data-testid="screen-notice-text" className="min-w-0 flex-1 whitespace-nowrap">
          {notice === "reconnect" ? <><span className="md:hidden">{t("screen.reconnectStoppedShort")}</span><span className="hidden md:inline">{t("screen.reconnectStopped")}</span></> : t(title)}
        </p>
        {notice === "reconnect" ? (
          <Button size="sm" className="h-7 whitespace-nowrap px-2 text-xs" variant="primary" aria-label={t("screen.shareAgain")} disabled={phase !== "online" || !!own || channel === null} onClick={() => controller.screen.beginShare()}>
            <span className="min-[380px]:hidden">{t("screen.shareAgainShort")}</span><span className="hidden min-[380px]:inline">{t("screen.shareAgain")}</span>
          </Button>
        ) : (
          <Button size="sm" className="h-7 whitespace-nowrap px-2 text-xs" onClick={() => setHelp(true)}>{t(notice === "permissionHint" ? "screen.permissionHintFix" : "screen.permissionFix")}</Button>
        )}
        <IconButton label={t("common.close")} size="sm" tooltip={false} onClick={() => useScreen.getState().dispatch({ type: "notice", notice: null })}><X className="size-4" /></IconButton>
      </div>
      {notice !== "reconnect" && <Dialog open={help} onOpenChange={setHelp} title={t(notice === "permissionHint" ? "screen.permissionHintFix" : "screen.permissionFix")} footer={
        canOpenScreenRecordingSettings() ? <Button onClick={() => void openScreenRecordingSettings().catch((e: unknown) => console.warn("[screen] privacy settings:", e))}>{t("screen.openSettings")}</Button> : undefined
      }>
        <p className="text-sm leading-relaxed text-pretty text-muted">{t(notice === "permissionHint" ? "screen.permissionHint" : "screen.permission", {
          app: captureApp(isDesktop(), navigator.userAgent) ?? t("screen.browser"), settings: t(`screen.permissionPlace.${captureOs(navigator.userAgent)}`),
        })}</p>
      </Dialog>}
    </>
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
  if (!own || (own.phase !== "starting" && own.phase !== "live" && own.phase !== "reconnecting")) return null;
  const watching = own.phase === "live" ? t(countKey(lang, "screen.watching", own.viewers), { count: own.viewers }) : t(own.phase === "reconnecting" ? "screen.reconnecting" : "screen.starting");
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
    <details data-testid="screen-sender-stats" className="relative text-xs text-white">
      <summary className="cursor-pointer rounded border border-white/40 px-2 py-1 font-medium">
        {t(stats.limitation === "cpu" ? "screen.limitedCpu" : stats.limitation === "bandwidth" ? "screen.limitedBandwidth" : "screen.qualityGood")}
      </summary>
      <div className="absolute top-full left-0 z-50 mt-1 w-64 rounded-lg border border-line-strong bg-raised p-3 text-fg shadow-pop">
        <p className="mb-1 font-medium">{t("common.details")}</p>
        <p className="tabular-nums">{`${stats.height}p · ${stats.fps} fps · ${mbps} Mb/s${stats.codec ? ` · ${stats.codec}` : ""}`}</p>
        {encoder && <p className="mt-1 text-muted">{encoder}</p>}
      </div>
    </details>
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

// Linux WebKit can stop supplying a replacement player's frames/metadata even while
// its receiver keeps decoding. Keep one attached player per slot stream and move it
// between views. A new transport supplies new streams, so it cannot inherit old pixels.
type SlotGrant = { run: number; id: number; received: boolean; repair: boolean; after?: { time: number } };
type SlotVideo = { video: HTMLVideoElement; grant?: SlotGrant };
const slotVideos = new Map<MediaStream, SlotVideo>();
function assignSlotVideo(player: SlotVideo, run: number, id: number): SlotGrant {
  if (player.grant?.run === run && player.grant.id === id) return player.grant;
  // Snapshot at the grant, before React moves the player. Its cached pixels and
  // readiness belong to the previous watch, even though the stream is unchanged.
  const after = player.grant ? { time: performance.now() } : undefined;
  return player.grant = { run, id, received: false, repair: false, after };
}
let slotVideoParking: HTMLDivElement | null = null;
function parkSlotVideo(video: HTMLVideoElement) {
  if (!slotVideoParking) {
    slotVideoParking = document.createElement("div");
    slotVideoParking.dataset.screenPlayers = "";
    slotVideoParking.setAttribute("aria-hidden", "true");
    // A negative layer behind the document can stall WebKit's idle players at
    // HAVE_NOTHING. Keep their renderers present; hide the parent, not each video.
    slotVideoParking.style.cssText = "position:fixed;top:0;left:0;width:16px;height:16px;display:grid;grid-template: minmax(0,1fr) / minmax(0,1fr);overflow:hidden;pointer-events:none;opacity:0;z-index:0";
    const cover = document.createElement("div");
    cover.style.cssText = "position:absolute;inset:0;background:black;z-index:1";
    slotVideoParking.appendChild(cover);
    document.body.appendChild(slotVideoParking);
  }
  video.removeAttribute("data-share");
  video.className = "size-full object-contain";
  video.style.gridArea = "1 / 1";
  slotVideoParking.prepend(video);
}
function prepareSlotVideo(stream: MediaStream | null) {
  const video = document.createElement("video");
  video.muted = true;
  video.autoplay = true;
  video.playsInline = true;
  const player: SlotVideo = { video };
  if (stream) slotVideos.set(stream, player);
  return player;
}
useScreen.subscribe(screen => {
  for (const [stream, player] of slotVideos) {
    const slot = screen.streams.indexOf(stream);
    if (slot !== -1) {
      const watch = screen.watches.find(w => w.slot === slot && w.status === "watching");
      if (watch) assignSlotVideo(player, screen.transport.run, watch.id);
      continue;
    }
    player.video.srcObject = null;
    player.video.remove();
    slotVideos.delete(stream);
  }
  if (slotVideos.size === 0) { slotVideoParking?.remove(); slotVideoParking = null; }
});

/**
 * A watched share's video, from its slot's stream, and why the server holds it back while it
 * does (`compact` on a card: the reason only).
 */
export function ShareVideo({ share, className, onDoubleClick, videoRef, compact }: { share: string; className?: string; onDoubleClick?: () => void; videoRef?: (video: HTMLVideoElement | null) => void; compact?: boolean }) {
  const t = useT();
  const run = useScreen(s => s.transport.run);
  const paused = useScreen(s => s.watches.find(w => w.share === share && w.status === "watching")?.paused);
  const watchId = useScreen(s => s.watches.find(w => w.share === share)?.id);
  const stream = useScreen(s => { const slot = slotOf(s, share); return slot === null ? null : (s.streams[slot] ?? null); });
  const host = useRef<HTMLDivElement | null>(null);
  const [shown, setShown] = useState<{ stream: MediaStream; run: number; id: number } | null>(null);
  const showing = stream !== null && shown?.stream === stream && shown.run === run && shown.id === watchId;
  useLayoutEffect(() => {
    const mount = host.current;
    if (!mount) return;
    let player = stream ? slotVideos.get(stream) : undefined;
    if (!player) player = prepareSlotVideo(stream);
    const { video } = player;
    const grant = watchId === undefined ? undefined : assignSlotVideo(player, run, watchId);
    let live = true, probe: ReturnType<typeof setTimeout> | undefined, frame: number | undefined;
    const hasFrame = () => video.readyState >= (HTMLMediaElement.HAVE_CURRENT_DATA ?? 2);
    video.dataset.share = share;
    video.className = "size-full object-contain";
    const received = (event?: Event, fresh = false) => {
      if (!live || !stream || !grant || player.grant !== grant || (event?.type === "resize" && !hasFrame())) return;
      if (grant.after && !grant.received && !fresh) return;
      grant.received = true;
      clearTimeout(probe);
      setShown({ stream, run, id: grant.id });
      controller.screen.frameReceived(share, run, grant.id);
    };
    const presented: VideoFrameRequestCallback = (_, metadata) => {
      if (!live || !grant || player.grant !== grant || grant.received) return;
      const after = grant.after!;
      // A delayed callback may still describe the old frame. Prefer its network
      // receipt time; otherwise use the frame's original presentation timestamp,
      // not the callback's invocation time. WebKit resets playback-quality counts
      // on a player move, but a cached frame keeps its pre-grant timestamp.
      const fresh = (metadata.receiveTime ?? metadata.presentationTime) > after.time;
      if (fresh) received(undefined, true);
      else frame = video.requestVideoFrameCallback(presented);
    };
    const repair = async () => {
      if (!live || !grant || player.grant !== grant || grant.received || grant.repair) return;
      const stats = await controller.screen.stats(share).catch(() => null);
      if (!live || player.grant !== grant || grant.received) return;
      if (!grant.after && hasFrame()) { received(); return; }
      if (stats && stats.framesDecoded > 0) {
        // WebKit's media-player decoder can miss the receiver's startup keyframe
        // while selecting its stream. Ask once AFTER input exists, rather than
        // during attachment. Repair never requires a second fresh frame to reveal.
        grant.repair = true;
        controller.screen.requestFrame(share, run, grant.id);
      } else probe = setTimeout(() => void repair(), 100);
    };
    video.addEventListener("loadeddata", received);
    video.addEventListener("resize", received);
    if (onDoubleClick) video.addEventListener("dblclick", onDoubleClick);
    mount.appendChild(video);
    videoRef?.(video);
    if (video.srcObject !== stream) {
      // Initialize WebKit's renderer in its mounted host before binding media.
      void video.offsetWidth;
      video.srcObject = stream;
    }
    if (stream) void video.play().catch(() => {});
    // Static readiness survives view moves only within the same grant. Reuse
    // keeps the binding but covers its old frame until a post-grant frame arrives.
    if (grant?.received || (!grant?.after && hasFrame())) received();
    if (stream && grant && !grant.received) {
      if (grant.after && typeof video.requestVideoFrameCallback === "function") frame = video.requestVideoFrameCallback(presented);
      probe = setTimeout(() => void repair(), 100);
    }
    return () => {
      live = false;
      clearTimeout(probe);
      if (frame !== undefined) video.cancelVideoFrameCallback(frame);
      video.removeEventListener("loadeddata", received);
      video.removeEventListener("resize", received);
      if (onDoubleClick) video.removeEventListener("dblclick", onDoubleClick);
      videoRef?.(null);
      const screen = useScreen.getState();
      if (document.pictureInPictureElement === video && !screen.watches.some(w => w.id === watchId && w.share === share && w.status === "watching")) {
        void document.exitPictureInPicture().catch(() => {});
      }
      if (video.parentElement === mount) {
        if (stream && screen.streams.includes(stream)) parkSlotVideo(video);
        else video.remove();
      }
      // Keep the binding through view/StrictMode remounts. The stream's retirement
      // above detaches it and bounds the cache to the run's receive slots.
    };
  }, [stream, share, run, watchId, onDoubleClick, videoRef]);
  return (
    <div data-screen-video data-ready={showing} className={cn("relative flex min-h-0 items-center justify-center overflow-hidden bg-black", className)}>
      <div ref={host} className="contents" />
      {!showing && !paused && (
        <div data-screen-placeholder className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center bg-black text-sm text-white/70">{t("screen.connecting")}</div>
      )}
      {paused && <PausedOverlay share={share} reason={paused} compact={compact} />}
    </div>
  );
}

/** A Try again the server has not answered this long after it was due is offered again. */
const RETRY_ANSWER_MS = 10_000;

/** Over the last frame, while the server holds a watched share back for my connection or its budget. */
function PausedOverlay({ share, reason, compact }: { share: string; reason: ScreenPauseReason; compact?: boolean }) {
  const t = useT();
  // The server stopped retrying on its own: only Try again brings the picture back now.
  const waiting = useScreen(s => s.watches.find(w => w.share === share)?.waiting) && reason === "bandwidth";
  // When the server carries out my Try again (it holds one asked too soon), and a clock to count down to it.
  const [due, setDue] = useState<number | null>(null);
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (due === null) return;
    const tick = setInterval(() => setNow(Date.now()), 250);
    // Resuming unmounts this; without an answer (it resumed and paused again unseen, or the server had nothing to retry), offer it again.
    const expire = setTimeout(() => setDue(null), Math.max(0, due - Date.now()) + RETRY_ANSWER_MS);
    return () => { clearInterval(tick); clearTimeout(expire); };
  }, [due]);
  const text = t(waiting ? "screen.pausedWaiting" : reason === "bandwidth" ? "screen.pausedBandwidth" : "screen.pausedBudget");
  const invite = waiting && due === null;
  const left = due === null ? 0 : Math.ceil((due - now) / 1000);
  return (
    <div data-screen-paused={reason} data-waiting={waiting || undefined} role="status" className="absolute inset-0 z-10 flex flex-col items-center justify-center gap-3 bg-black/75 p-3 text-center text-white">
      {compact ? (
        <span className="rounded bg-white/15 px-1.5 text-[11px] leading-5 font-medium" title={text}>{t(reason === "bandwidth" ? "screen.pausedLink" : "screen.pausedShort")}</span>
      ) : (
        <>
          <p className="max-w-sm text-sm text-balance text-white/90">{text}</p>
          {reason === "bandwidth" && (
            <Button size="sm" variant={invite ? "primary" : "secondary"} disabled={due !== null}
              className={cn("tabular-nums disabled:opacity-80", !invite && "border-white/30 text-white hover:bg-white/10")}
              onClick={e => {
                e.stopPropagation();
                const wait = controller.screen.retry(share);
                if (wait === null) return;
                setNow(Date.now());
                setDue(Date.now() + wait);
              }}>
              {due !== null && left > 0 ? t("screen.retryIn", { seconds: left }) : (
                <><RotateCw className={cn("size-4", due !== null && "animate-spin")} /> {t(due === null ? "screen.retry" : "screen.retrying")}</>
              )}
            </Button>
          )}
        </>
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
  const t = useT();
  const paused = useScreen(s => s.watches.find(w => w.share === share)?.paused);
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
  // A paused slot's last resolution at 0 fps would read as a frozen stream.
  // On a phone the overlay below already says why; the title keeps the room.
  if (paused) return <span data-testid="screen-quality" className="shrink-0 rounded bg-white/10 px-1.5 text-[11px] leading-5 whitespace-nowrap text-white/80 max-sm:hidden">{t(paused === "bandwidth" ? "screen.pausedLink" : "screen.pausedShort")}</span>;
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
      // Not while Home covers the stage: it would go fullscreen unseen.
      if (useUi.getState().section !== "servers") return;
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
        {!card && <BackButton label={t("nav.toChannels")} entry onClick={() => paneBack("list")} />}
        {client && <TalkingAvatar name={name} seed={client.uid} size={24} talking={client.talking} />}
        <h2 className="min-w-0 truncate text-[15px] font-semibold">{info ? t("screen.sharerTitle", { name }) : t("screen.watchReconnecting")}</h2>
        {info && <span className="shrink-0 rounded bg-danger px-1 text-[10px] leading-4 font-bold tracking-wide">{t("screen.live")}</span>}
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
            <X className="size-4" /> <span className="max-sm:sr-only">{t("screen.stopWatching")}</span>
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
  if (watched) body = <ShareVideo share={share} className="aspect-video w-full" compact />;
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
