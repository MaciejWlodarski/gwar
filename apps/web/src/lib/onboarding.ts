/** Device-local preferences only; identity keys keep their existing format. */
export interface Onboarding {
  completed: boolean;
  firstUsedAt: number;
  nudgeDismissed: boolean;
  roleReceived: boolean;
}

export function readOnboarding(value: unknown): Onboarding | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Partial<Onboarding>;
  if (typeof v.firstUsedAt !== "number" || !Number.isFinite(v.firstUsedAt) || v.firstUsedAt <= 0) return null;
  return { firstUsedAt: v.firstUsedAt, completed: v.completed === true, nudgeDismissed: v.nudgeDismissed === true, roleReceived: v.roleReceived === true };
}

/** Safari gets the reminder after one day, other browsers after three. Roles trigger it immediately. */
export function identityNudgeDue(onboarding: Onboarding | null, signedIn: boolean, now: number, userAgent: string): boolean {
  if (!onboarding?.completed || onboarding.nudgeDismissed || signedIn) return false;
  const safari = /Safari\//.test(userAgent) && !/Chrome|Chromium|CriOS|Edg|OPR|Android/.test(userAgent);
  return onboarding.roleReceived || now - onboarding.firstUsedAt >= (safari ? 1 : 3) * 86_400_000;
}
