import { create } from "zustand";
import { useSession, useVoice } from "./stores";
import type { UplinkStatus } from "../voice/uplink-congestion";

/**
 * Ephemeral measurements, independently scoped to the two outbound transports, and the one
 * congestion notice per episode (docs/screen-share.md, "Connection quality").
 */
interface Uplink {
  voice: UplinkStatus | null;
  screen: UplinkStatus | null;
  /** The brief message of the current episode, until it collapses or is dismissed. */
  notice: { episode: number; delayMs: number } | null;
  /** The connection details dialog. */
  details: boolean;
  /** The colour on screen: `connectionQuality` with a better one held back (`noteQuality`). */
  shown: Quality | null;
}

export const useUplink = create<Uplink>(() => ({ voice: null, screen: null, notice: null, details: false, shown: null }));

export const voiceConnectionActive = (voice = useVoice.getState(), phase = useSession.getState().phase): boolean =>
  voice.state === "connected" && !voice.reconnecting && !voice.failure && phase === "online";

const congestion = (s: Uplink) => s.screen?.congested ? s.screen : s.voice?.congested ? s.voice : null;

export function useUploadCongestion(): UplinkStatus | null {
  return useUplink(congestion);
}

export const currentUploadCongestion = (): UplinkStatus | null => congestion(useUplink.getState());

export type Quality = "good" | "fair" | "poor";

export interface ConnectionQuality {
  quality: Quality;
  rttMs: number;
  lossPct: number | null;
  /** Set while the upload queue is the likely cause. */
  congestion: UplinkStatus | null;
  availableOutgoingBitrate: number | null;
}

/**
 * Round trip: poor from 300 ms, about 150 ms each way, up to which a conversation stays natural
 * (ITU-T G.114), so a far server alone is not flagged; loss in percent of what we send.
 */
const RTT_FAIR = 150, RTT_POOR = 300, LOSS_FAIR = 3, LOSS_POOR = 10;
/** Queueing delay others notice as a lag in conversation. */
const ADDED_POOR = 250;

/**
 * What a user experiences now, from measurements only: never from the detector's timers, so a
 * floor the detector adopted cannot turn a slow connection green. Media RTT is fresher than the
 * 10 s server ping and measures the path voice takes; the worst of the two transports counts.
 */
export function connectionQuality(serverRttMs: number | null, voice: UplinkStatus | null, screen: UplinkStatus | null): ConnectionQuality | null {
  const media = [voice, screen].filter((s): s is UplinkStatus => !!s);
  const rttMs = media.length ? Math.max(...media.map(s => s.rttMs)) : serverRttMs;
  if (rttMs === null) return null;
  const losses = media.map(s => s.lossPct).filter((l): l is number => typeof l === "number");
  const lossPct = losses.length ? Math.max(...losses) : null;
  const congested = screen?.congested ? screen : voice?.congested ? voice : null;
  const poor = rttMs >= RTT_POOR || (lossPct ?? 0) >= LOSS_POOR || (!!congested && congested.rttMs - congested.baselineMs >= ADDED_POOR);
  const fair = rttMs >= RTT_FAIR || (lossPct ?? 0) >= LOSS_FAIR || !!congested;
  const rates = media.map(s => s.availableOutgoingBitrate).filter((r): r is number => r !== null);
  return {
    quality: poor ? "poor" : fair ? "fair" : "good", rttMs, lossPct, congestion: congested,
    availableOutgoingBitrate: rates.length ? Math.min(...rates) : null,
  };
}

/**
 * A worse colour shows at once; a better one only once the connection has stayed better than it for
 * this long, so a round trip at a threshold does not flicker the colour or the pill's indicator. One
 * held colour for every view, so the pill and the panel never disagree.
 */
export const QUALITY_IMPROVE_MS = 4_000;
export const QUALITY_RANK: Record<Quality, number> = { good: 0, fair: 1, poor: 2 };
const LEVELS: Quality[] = ["good", "fair", "poor"];
let improveTimer: ReturnType<typeof setTimeout> | null = null;
/** Since when the connection has been better than each level without a break. */
const betterSince: Partial<Record<Quality, number>> = {};

const liveQuality = (): Quality | null => {
  const { voice, screen } = useUplink.getState();
  return connectionQuality(useSession.getState().rtt, voice, screen)?.quality ?? null;
};

function noteQuality(): void {
  if (improveTimer) clearTimeout(improveTimer);
  improveTimer = null;
  const live = liveQuality();
  const now = Date.now();
  for (const q of LEVELS) {
    if (live && QUALITY_RANK[live] < QUALITY_RANK[q]) betterSince[q] ??= now;
    else delete betterSince[q];
  }
  let shown = useUplink.getState().shown;
  // No measurement, the same, or worse: honest at once. Better: step down past every level the
  // connection has stayed better than for the whole hold.
  if (!live || !shown || QUALITY_RANK[live] >= QUALITY_RANK[shown]) shown = live;
  else {
    let q: Quality = shown;
    while (q !== live && (betterSince[q] ?? Infinity) <= now - QUALITY_IMPROVE_MS) q = LEVELS[QUALITY_RANK[q] - 1] ?? live;
    shown = q;
  }
  if (shown !== useUplink.getState().shown) useUplink.setState({ shown });
  if (shown && live && shown !== live) improveTimer = setTimeout(noteQuality, betterSince[shown]! + QUALITY_IMPROVE_MS - now);
}
useUplink.subscribe((s, prev) => {
  // A late stats callback from the previous call cannot seed the next call.
  if (s.voice && !voiceConnectionActive()) { useUplink.setState({ voice: null }); return; }
  if (s.voice !== prev.voice || s.screen !== prev.screen) noteQuality();
});
useSession.subscribe((s, prev) => { if (s.rtt !== prev.rtt) noteQuality(); });

/** A verdict that returns this soon after the last one ended continues that episode: no new message. */
export const EPISODE_MERGE_MS = 120_000;
let episode = 0;
let active = false;
let endedAt = -Infinity;
useUplink.subscribe(() => {
  const s = useUplink.getState();
  // The notice says others hear me late. A share alone cannot make that claim,
  // and resuming voice must wait for a measurement from the new call.
  const status = voiceConnectionActive() && s.voice?.congested ? s.voice : null;
  if (!!status === active) return;
  active = !!status;
  const now = Date.now();
  // A message about a queue that has drained would be stale.
  if (!status) { endedAt = now; if (s.notice) useUplink.setState({ notice: null }); return; }
  if (now - endedAt < EPISODE_MERGE_MS) return;
  episode++;
  useUplink.setState({ notice: { episode, delayMs: Math.max(0, status.rttMs - status.baselineMs) } });
});

function voiceConnectionChanged(): void {
  if (voiceConnectionActive()) return;
  active = false;
  endedAt = -Infinity;
  useUplink.setState({ voice: null, notice: null, details: false });
}
useVoice.subscribe((s, prev) => {
  if (s.state !== prev.state || s.reconnecting !== prev.reconnecting || s.failure !== prev.failure) voiceConnectionChanged();
});
useSession.subscribe((s, prev) => { if (s.phase !== prev.phase) voiceConnectionChanged(); });

export const dismissCongestionNotice = () => useUplink.setState({ notice: null });
export const openConnectionDetails = () => useUplink.setState({ details: true, notice: null });
export const closeConnectionDetails = () => useUplink.setState({ details: false });

/** Tests only: forget earlier episodes. */
export function resetCongestionEpisodes(): void {
  active = false; endedAt = -Infinity;
  if (improveTimer) clearTimeout(improveTimer);
  improveTimer = null;
  for (const q of LEVELS) delete betterSince[q];
  useUplink.setState({ voice: null, screen: null, notice: null, details: false, shown: null });
}
