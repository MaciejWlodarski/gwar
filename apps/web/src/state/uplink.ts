import { create } from "zustand";
import type { UplinkStatus } from "../voice/uplink-congestion";

interface Uplink { voice: UplinkStatus | null; screen: UplinkStatus | null }

/** Ephemeral measurements, independently scoped to the two outbound transports. */
export const useUplink = create<Uplink>(() => ({ voice: null, screen: null }));

const congestion = (s: Uplink) => s.screen?.congested ? s.screen : s.voice?.congested ? s.voice : null;

export function useUploadCongestion(): UplinkStatus | null {
  return useUplink(congestion);
}

export const currentUploadCongestion = (): UplinkStatus | null => congestion(useUplink.getState());
