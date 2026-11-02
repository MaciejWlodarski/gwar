/** One DOM snapshot: a new pending render cannot race a later flag query. */
export function verificationState(page) {
  return page.evaluate(() => ({
    pending: document.querySelectorAll('[data-message][data-message-verification="pending"]').length,
    flags: document.querySelectorAll('[data-testid^="message-verification-"]').length,
    signatureValid: [...document.querySelectorAll('body *')].some(element =>
      !['SCRIPT', 'STYLE'].includes(element.tagName) && element.textContent?.replace(/\s+/g, ' ').trim() === 'Signature valid'),
  }));
}

/** Signature results can be ready before an asynchronous channel-history read is fresh.
 * Keep the original 15-second verification deadline and return any unresolved warning at it. */
export async function settledVerificationState(page, { timeout = 15_000 } = {}) {
  const deadline = performance.now() + timeout;
  for (;;) {
    const state = await verificationState(page);
    if (state.pending === 0 && state.flags === 0 && !state.signatureValid) return state;
    const remaining = deadline - performance.now();
    if (remaining <= 0) return state;
    await new Promise(resolve => setTimeout(resolve, Math.min(100, remaining)));
  }
}
