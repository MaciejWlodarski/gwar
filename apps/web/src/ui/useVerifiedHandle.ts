import { useEffect, useState } from "react";
import { loadServiceKey } from "../connect/service-key";
import { attestationFingerprint, verifiedHandle, type HandleMember } from "../net/handle-attestation";

/** A stale async result never enables a badge for a different member or attestation. `verifiedHandle` checks the
 *  attestation's times when it verifies; the timer withdraws the badge when it expires. */
export function useVerifiedHandle(member: HandleMember | null | undefined): string | null {
  const a = member?.connect;
  const uid = member?.uid;
  const fingerprint = a && typeof a === "object" ? attestationFingerprint(a) : "";
  const [result, setResult] = useState<{ uid: string; fingerprint: string; handle: string | null } | null>(null);
  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    void loadServiceKey().then(async (key) => {
      if (!a || !uid) return;
      const handle = await verifiedHandle({ uid, connect: a }, key, Date.now());
      if (!active) return;
      setResult({ uid, fingerprint, handle });
      if (handle) {
        timer = setTimeout(() => {
          if (active) setResult(null);
        }, Math.min(2_147_483_647, Math.max(0, a.expires_at - Date.now())));
      }
    });
    return () => { active = false; clearTimeout(timer); };
  }, [a, uid, fingerprint]);
  return a && typeof a === "object" && result !== null && result.uid === uid && result.fingerprint === fingerprint ? result.handle : null;
}
