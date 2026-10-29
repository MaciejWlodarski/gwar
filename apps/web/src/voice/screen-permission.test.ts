import { describe, expect, it } from "vitest";
import { captureApp, captureFailure, captureOs } from "./screen-permission";

describe("screen capture errors", () => {
  it.each(["Permission denied", "User cancelled the picker"])("keeps cancellation silent: %s", message => {
    expect(captureFailure(new DOMException(message, "NotAllowedError"), "Chrome/153")).toBe("cancelled");
  });
  it.each(["Permission denied by system", "Permission denied by policy", "Screen recording denied"])("identifies OS refusal: %s", message => {
    expect(captureFailure(new DOMException(message, "NotAllowedError"))).toBe("permission");
  });
  it("uses generic guidance for other failures and ignores AbortError", () => {
    expect(captureFailure(new DOMException("cancelled", "AbortError"))).toBe("cancelled");
    expect(captureFailure(new Error("raw browser details"))).toBe("failed");
    expect(captureFailure(new DOMException("Permission denied by system", "NotReadableError"))).toBe("permission");
  });
  it.each(["Version/26 Safari/605", "Firefox/153", "unknown"])("uses a neutral hint for ambiguous user-agent/platform refusal: %s", agent => {
    expect(captureFailure(new DOMException("The request is not allowed by the user agent or the platform in the current context.", "NotAllowedError"), agent)).toBe("hint");
    expect(captureFailure(new DOMException("The request is not allowed", "NotAllowedError"), agent)).toBe("hint");
    expect(captureFailure(new DOMException("User cancelled the picker", "NotAllowedError"), agent)).toBe("cancelled");
  });
  it("selects guidance for the actual OS", () => {
    expect(captureOs("Macintosh; Intel Mac OS X 10_15_7")).toBe("macos");
    expect(captureOs("Windows NT 10.0")).toBe("windows");
    expect(captureOs("Linux")).toBe("other");
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
