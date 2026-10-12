import { useEffect, useSyncExternalStore } from "react";
import type { HandleMember } from "../net/handle-attestation";
import { handlePinsVersion, handleStatus, rememberHandle, subscribeHandlePins, type HandleStatus } from "../net/handle-pins";
import { useVerifiedHandle } from "./useVerifiedHandle";

export interface ConnectHandle {
  handle: string;
  /** "changed": this client saw the handle on a different account before (net/handle-pins.ts). */
  status: HandleStatus;
}

/** A verified handle together with its continuity status. The first verified sighting pins it; until then nothing
 *  is shown, so two accounts showing the same new handle at once are never both verified. */
export function useConnectHandle(member: HandleMember | null | undefined): ConnectHandle | null {
  const handle = useVerifiedHandle(member);
  const uid = member?.uid;
  useSyncExternalStore(subscribeHandlePins, handlePinsVersion);
  useEffect(() => {
    if (handle && uid) rememberHandle(handle, uid);
  }, [handle, uid]);
  if (!handle || !uid) return null;
  const status = handleStatus(handle, uid);
  return status === "pending" ? null : { handle, status };
}
