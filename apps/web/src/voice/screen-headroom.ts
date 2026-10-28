/** Opus plus RTCP and repairs, shared with the local startup diagnostic. */
export const VOICE_RESERVE = 200_000;
/** Keep a larger repair margin once the estimate exceeds 2 Mb/s. */
const REPAIR_RESERVE_SHARE = 0.1;
/** Below this floor, GCC must recover by probing; voice headroom is no longer guaranteed. */
export const MIN_BITRATE = 250_000;

/** Video's budget under a finite, positive uplink estimate, before the spec cap. */
export function uplinkVideoBudget(available: number): number {
  return Math.max(MIN_BITRATE, Math.floor(available - Math.max(VOICE_RESERVE, REPAIR_RESERVE_SHARE * available)));
}
