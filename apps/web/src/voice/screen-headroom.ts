/** Opus plus RTCP and repairs, shared with the local startup diagnostic. */
export const VOICE_RESERVE = 200_000;
/** Keep a larger repair margin once the estimate exceeds 2 Mb/s. */
const REPAIR_RESERVE_SHARE = 0.1;
/**
 * Never reserve more of the estimate than this. Chromium's GCC raises its estimate to at most
 * 1.5 times the throughput it measures, so video held under 2/3 of the estimate can never let
 * it grow: after congestion a low estimate would stay where it fell (docs/latency-congested-uplink.md).
 */
const MAX_RESERVE_SHARE = 0.3;
/** Below this floor, GCC must recover by probing; voice headroom is no longer guaranteed. */
export const MIN_BITRATE = 250_000;

/** Video's budget under a finite, positive uplink estimate, before the spec cap. */
export function uplinkVideoBudget(available: number): number {
  const reserve = Math.min(Math.max(VOICE_RESERVE, REPAIR_RESERVE_SHARE * available), MAX_RESERVE_SHARE * available);
  return Math.max(MIN_BITRATE, Math.floor(available - reserve));
}
