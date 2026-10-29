/** OS-specific guidance uses only known names, never raw browser errors or a raw UA. */
export function captureOs(userAgent: string): "macos" | "windows" | "other" {
  return !/iPhone|iPad/.test(userAgent) && /Macintosh|Mac OS X/.test(userAgent) ? "macos" : /Windows/.test(userAgent) ? "windows" : "other";
}

/**
 * Chromium identifies picker cancellation with "Permission denied" and OS refusal with
 * "by system". WebKit/Firefox often use the same NotAllowedError for user and platform
 * refusals: retain a neutral hint for those ambiguous errors, rather than hiding a refusal.
 */
export function captureFailure(error: unknown, userAgent = globalThis.navigator?.userAgent ?? ""): "cancelled" | "permission" | "hint" | "failed" {
  if (!(error instanceof Error)) return "failed";
  if (error.name === "AbortError" || /cancel(?:led|ed)?|dismiss(?:ed)?/i.test(error.message)) return "cancelled";
  if (/system|policy|denied.*(?:os|operating)|screen.?recording/i.test(error.message)
    && /denied|permission|not allowed|blocked/i.test(error.message)) return "permission";
  if (error.name === "NotAllowedError") {
    if (/Chrome\/|Edg\//.test(userAgent) && /^Permission denied\.?$/i.test(error.message)) return "cancelled";
    return "hint";
  }
  return "failed";
}

export function captureApp(desktop: boolean, userAgent: string): string | null {
  if (desktop) return "Gwar";
  if (/Edg\//.test(userAgent)) return "Microsoft Edge";
  if (/Firefox\//.test(userAgent)) return "Firefox";
  if (/Chrome\//.test(userAgent)) return "Google Chrome";
  return /Safari\//.test(userAgent) ? "Safari" : null;
}
