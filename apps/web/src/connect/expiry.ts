/** When to tell the person that this device's certificate needs the password (docs/connect.md, "Signed statements"). */

/** Warn when the certificate has this many days left or fewer. */
export const WARN_DAYS = 30;

export type CertificateWarning =
  | { level: "none" }
  | { level: "expiring"; daysLeft: number }
  | { level: "expired" };

/**
 * `checked` is true once the start-up check for a newer certificate (another
 * device may have renewed this one) has finished, whatever it found; before
 * that the stored expiry may be about to change, so nothing is shown yet.
 */
export function certificateWarning(expiresAt: number, now: number, checked: boolean): CertificateWarning {
  if (!checked) return { level: "none" };
  if (expiresAt <= now) return { level: "expired" };
  const daysLeft = Math.ceil((expiresAt - now) / 86_400_000);
  return daysLeft <= WARN_DAYS ? { level: "expiring", daysLeft } : { level: "none" };
}
