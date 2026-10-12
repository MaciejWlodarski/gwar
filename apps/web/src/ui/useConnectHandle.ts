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
  const status = handle && uid ? handleStatus(handle, uid) : null;
  // Runs again when the pin disappears (another tab cleared or evicted it), so a shown handle never stays hidden.
  const pending = status === "pending";
  useEffect(() => {
    if (handle && uid) void rememberHandle(handle, uid);
  }, [handle, uid, pending]);
  return !handle || !status || status === "pending" ? null : { handle, status };
}
