import { describe, expect, it } from "vitest";
import { captureApp, captureFailure } from "./screen-permission";

describe("screen capture errors", () => {
  it.each(["Permission denied", "The request is not allowed", "User cancelled the picker"])("keeps cancellation silent: %s", message => {
    expect(captureFailure(new DOMException(message, "NotAllowedError"))).toBe("cancelled");
  });
  it.each(["Permission denied by system", "Permission denied by policy", "Screen recording denied"])("identifies OS refusal: %s", message => {
    expect(captureFailure(new DOMException(message, "NotAllowedError"))).toBe("permission");
  });
  it("uses generic guidance for other failures and ignores AbortError", () => {
    expect(captureFailure(new DOMException("cancelled", "AbortError"))).toBe("cancelled");
    expect(captureFailure(new Error("raw browser details"))).toBe("failed");
    expect(captureFailure(new DOMException("Permission denied by system", "NotReadableError"))).toBe("permission");
  });
  it("names only known browsers or Gwar", () => {
    expect(captureApp(true, "anything")).toBe("Gwar");
    expect(captureApp(false, "Chrome/1 Safari/1 Edg/1")).toBe("Microsoft Edge");
    expect(captureApp(false, "Chrome/1 Safari/1")).toBe("Google Chrome");
    expect(captureApp(false, "Firefox/1")).toBe("Firefox");
    expect(captureApp(false, "Version/1 Safari/1")).toBe("Safari");
    expect(captureApp(false, "unknown")).toBeNull();
  });
});
