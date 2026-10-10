import { describe, expect, it } from "vitest";
import { certificateWarning, WARN_DAYS } from "./expiry";

const DAY = 86_400_000;
const now = 1_760_000_000_000;

describe("certificateWarning", () => {
  it("is silent while the certificate has plenty of time", () => {
    expect(certificateWarning(now + (WARN_DAYS + 1) * DAY, now, true)).toEqual({ level: "none" });
    expect(certificateWarning(now + 300 * DAY, now, true)).toEqual({ level: "none" });
  });

  it("warns from 30 days before the end, counting days left", () => {
    expect(certificateWarning(now + WARN_DAYS * DAY, now, true)).toEqual({ level: "expiring", daysLeft: 30 });
    expect(certificateWarning(now + 2 * DAY + 1, now, true)).toEqual({ level: "expiring", daysLeft: 3 });
    expect(certificateWarning(now + 1, now, true)).toEqual({ level: "expiring", daysLeft: 1 });
  });

  it("waits for the look at newer certificates: another device may have renewed this one", () => {
    expect(certificateWarning(now + 5 * DAY, now, false)).toEqual({ level: "none" });
    expect(certificateWarning(now - DAY, now, false)).toEqual({ level: "none" });
  });

  it("reports an expired certificate, from the exact moment it ends", () => {
    expect(certificateWarning(now - 1, now, true)).toEqual({ level: "expired" });
    expect(certificateWarning(now, now, true)).toEqual({ level: "expired" });
  });
});
