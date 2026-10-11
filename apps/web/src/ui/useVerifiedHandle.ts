import { useEffect, useState } from "react";
import { loadServiceKey } from "../connect/service-key";
import { attestationCurrent, attestationFingerprint, verifiedHandle, type HandleMember } from "../net/handle-attestation";

/** How often a shown badge rechecks the clock, which can jump (sleep, a changed clock) while no timer fires. */
const RECHECK_MS = 30_000;

/** A stale async result never enables a badge for a different member or attestation, nor outside the attestation's
 *  validity window: the window is checked after verification, on every render, at expiry and every RECHECK_MS. */
export function useVerifiedHandle(member: HandleMember | null | undefined): string | null {
  const a = member?.connect;
  const uid = member?.uid;
  const fingerprint = a && typeof a === "object" ? attestationFingerprint(a) : "";
  const [result, setResult] = useState<{ uid: string; fingerprint: string; handle: string | null } | null>(null);
  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let recheck: ReturnType<typeof setInterval> | undefined;
    void loadServiceKey().then(async (key) => {
      if (!a || !uid) return;
      const handle = await verifiedHandle({ uid, connect: a }, key, Date.now());
      if (!active) return;
      // Verification takes time; the attestation may have expired meanwhile.
      const current = handle !== null && attestationCurrent(a, Date.now());
      setResult({ uid, fingerprint, handle: current ? handle : null });
      if (current) {
        const withdraw = () => { if (active) setResult(null); };
        timer = setTimeout(withdraw, Math.min(2_147_483_647, Math.max(0, a.expires_at - Date.now())));
        recheck = setInterval(() => { if (!attestationCurrent(a, Date.now())) withdraw(); }, RECHECK_MS);
      }
    });
    return () => { active = false; clearTimeout(timer); clearInterval(recheck); };
  }, [a, uid, fingerprint]);
  if (!a || typeof a !== "object" || result === null || result.uid !== uid || result.fingerprint !== fingerprint) return null;
  // Reading the clock while rendering is the point: a render after a clock jump must not show an invalid badge.
  // eslint-disable-next-line react-hooks/purity
  return attestationCurrent(a, Date.now()) ? result.handle : null;
}
