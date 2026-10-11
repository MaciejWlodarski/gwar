import { useEffect, useState } from "react";
import { loadServiceKey } from "../connect/service-key";
import { attestationCurrent, attestationFingerprint, verifiedHandle, type HandleMember } from "../net/handle-attestation";

/** How often a shown badge rechecks the clock, which can jump (sleep, a changed clock) while no timer fires. Browsers
 *  may delay timers in the background, so the window is also rechecked when the page comes back. */
const RECHECK_MS = 30_000;
const RESUME_EVENTS = ["focus", "pageshow"] as const;

/** A stale async result never enables a badge for a different member or attestation, nor outside the attestation's
 *  validity window: the window is checked after verification, on every render, at expiry, every RECHECK_MS while
 *  the badge is shown, and when the page becomes visible or focused again. */
export function useVerifiedHandle(member: HandleMember | null | undefined): string | null {
  const a = member?.connect;
  const uid = member?.uid;
  const fingerprint = a && typeof a === "object" ? attestationFingerprint(a) : "";
  const [result, setResult] = useState<{ uid: string; fingerprint: string; handle: string | null } | null>(null);
  useEffect(() => {
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let recheck: ReturnType<typeof setInterval> | undefined;
    let check: (() => void) | undefined;
    const stop = () => {
      clearTimeout(timer); clearInterval(recheck);
      if (check) {
        for (const name of RESUME_EVENTS) window.removeEventListener(name, check);
        document.removeEventListener("visibilitychange", check);
      }
    };
    void loadServiceKey().then(async (key) => {
      if (!a || !uid) return;
      const handle = await verifiedHandle({ uid, connect: a }, key, Date.now());
      if (!active) return;
      // Verification takes time; the attestation may have expired meanwhile.
      const current = handle !== null && attestationCurrent(a, Date.now());
      setResult({ uid, fingerprint, handle: current ? handle : null });
      if (current) {
        // Once withdrawn, nothing is left to recheck until the attestation or member changes.
        const withdraw = () => { stop(); if (active) setResult(null); };
        check = () => { if (!attestationCurrent(a, Date.now())) withdraw(); };
        timer = setTimeout(withdraw, Math.min(2_147_483_647, Math.max(0, a.expires_at - Date.now())));
        recheck = setInterval(check, RECHECK_MS);
        for (const name of RESUME_EVENTS) window.addEventListener(name, check);
        document.addEventListener("visibilitychange", check);
      }
    });
    return () => { active = false; stop(); };
  }, [a, uid, fingerprint]);
  if (!a || typeof a !== "object" || result === null || result.uid !== uid || result.fingerprint !== fingerprint) return null;
  // Reading the clock while rendering is the point: a render after a clock jump must not show an invalid badge.
  // eslint-disable-next-line react-hooks/purity
  return attestationCurrent(a, Date.now()) ? result.handle : null;
}
