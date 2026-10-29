/** Browsers use NotAllowedError for both picker cancellation and OS refusal. */
export function captureFailure(error: unknown): "cancelled" | "permission" | "failed" {
  if (!(error instanceof Error)) return "failed";
  if (error.name === "AbortError") return "cancelled";
  if (/system|policy|denied.*(?:os|operating)|screen.?recording/i.test(error.message)
    && /denied|permission|not allowed|blocked/i.test(error.message)) return "permission";
  if (error.name === "NotAllowedError") return "cancelled";
  return "failed";
}

/** Only known product names enter permission guidance, never a browser error or a raw UA. */
export function captureApp(desktop: boolean, userAgent: string): string | null {
  if (desktop) return "Gwar";
  if (/Edg\//.test(userAgent)) return "Microsoft Edge";
  if (/Firefox\//.test(userAgent)) return "Firefox";
  if (/Chrome\//.test(userAgent)) return "Google Chrome";
  return /Safari\//.test(userAgent) ? "Safari" : null;
}
