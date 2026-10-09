import { describe, expect, it } from "vitest";
import { formatRelative } from "./time";

const now = Date.UTC(2026, 5, 1, 12, 0, 0);

describe("formatRelative", () => {
  it("picks the largest fitting unit", () => {
    expect(formatRelative(now - 5_000, now, "en")).toBe("now");
    expect(formatRelative(now - 5 * 60_000, now, "en")).toBe("5 minutes ago");
    expect(formatRelative(now - 3 * 3_600_000, now, "en")).toBe("3 hours ago");
    expect(formatRelative(now - 86_400_000, now, "en")).toBe("yesterday");
    expect(formatRelative(now - 10 * 86_400_000, now, "en")).toBe("10 days ago");
  });
  it("never reports the future", () => {
    expect(formatRelative(now + 60_000, now, "en")).toBe("now");
  });
  it("follows the language", () => {
    expect(formatRelative(now - 2 * 86_400_000, now, "pl")).toBe("przedwczoraj");
  });
});
