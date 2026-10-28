import { create } from "zustand";
import type { UplinkStatus } from "../voice/uplink-congestion";

/** Ephemeral measurements, independently scoped to the two outbound transports. */
export const useUplink = create<{ voice: UplinkStatus | null; screen: UplinkStatus | null }>(() => ({ voice: null, screen: null }));

export function useUploadCongestion(): UplinkStatus | null {
  return useUplink(s => s.screen?.congested ? s.screen : s.voice?.congested ? s.voice : null);
}
