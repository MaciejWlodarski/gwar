import { useEffect, useSyncExternalStore } from "react";
import type { HandleMember } from "../net/handle-attestation";
import { handlePinsVersion, handleStatus, rememberHandle, subscribeHandlePins, type HandleStatus } from "../net/handle-pins";
import { useVerifiedHandle } from "./useVerifiedHandle";

export interface ConnectHandle {
  handle: string;
  /** "changed": this client saw the handle on a different account before (net/handle-pins.ts). */
  status: HandleStatus;
}

/** A verified handle together with its continuity status; the first verified sighting pins it. */
export function useConnectHandle(member: HandleMember | null | undefined): ConnectHandle | null {
  const handle = useVerifiedHandle(member);
  const uid = member?.uid;
  useSyncExternalStore(subscribeHandlePins, handlePinsVersion);
  useEffect(() => {
    if (handle && uid) rememberHandle(handle, uid);
  }, [handle, uid]);
  if (!handle || !uid) return null;
  return { handle, status: handleStatus(handle, uid) };
}
