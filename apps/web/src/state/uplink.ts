import { create } from "zustand";
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
}

export const useUplink = create<Uplink>(() => ({ voice: null, screen: null, notice: null, details: false }));

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
 * Round trip: a conversation stays natural up to ~150 ms one way (ITU-T G.114), so a far server
 * at 120 ms is not "fair"; loss in percent of what we send.
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

/** A verdict that returns this soon after the last one ended continues that episode: no new message. */
export const EPISODE_MERGE_MS = 120_000;
let episode = 0;
let active = false;
let endedAt = -Infinity;
useUplink.subscribe(s => {
  const status = congestion(s);
  if (!!status === active) return;
  active = !!status;
  const now = Date.now();
  // A message about a queue that has drained would be stale.
  if (!status) { endedAt = now; if (s.notice) useUplink.setState({ notice: null }); return; }
  if (now - endedAt < EPISODE_MERGE_MS) return;
  episode++;
  useUplink.setState({ notice: { episode, delayMs: Math.max(0, status.rttMs - status.baselineMs) } });
});

export const dismissCongestionNotice = () => useUplink.setState({ notice: null });
export const openConnectionDetails = () => useUplink.setState({ details: true, notice: null });
export const closeConnectionDetails = () => useUplink.setState({ details: false });

/** Tests only: forget earlier episodes. */
export function resetCongestionEpisodes(): void {
  active = false; endedAt = -Infinity;
  useUplink.setState({ voice: null, screen: null, notice: null, details: false });
}
