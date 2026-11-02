/** One DOM snapshot: a new pending render cannot race a later flag query. */
function observeVerification(page) {
  return page.evaluate(() => {
    const indicators = [...document.querySelectorAll('[data-testid^="message-verification-"]')].map(mark => ({
      kind: mark.getAttribute('data-testid'),
      label: mark.getAttribute('aria-label'),
      message: mark.closest('[data-message]')?.getAttribute('data-message') ?? null,
      continuity: mark.getAttribute('data-verification-continuity'),
      authorization: mark.getAttribute('data-verification-authorization'),
    }));
    const state = {
      pending: document.querySelectorAll('[data-message][data-message-verification="pending"]').length,
      flags: indicators.length,
      signatureValid: [...document.querySelectorAll('body *')].some(element =>
        !['SCRIPT', 'STYLE'].includes(element.tagName) && element.textContent?.replace(/\s+/g, ' ').trim() === 'Signature valid'),
    };
    // Only the observed asynchronous lifecycle qualification may still settle. Invalid,
    // unsigned, revoked, expired or unproven results must fail on this very snapshot.
    const waiting = indicators.every(mark => mark.kind === 'message-verification-pending'
      || (mark.kind === 'message-verification-qualified' && mark.continuity === 'proven'
        && mark.authorization === 'channel-history-incomplete'));
    return { state, waiting, indicators };
  });
}

export async function verificationState(page) { return (await observeVerification(page)).state; }

/** Keep the existing 15-second deadline for pending signatures/lifecycle reads. A real
 * warning is returned immediately, together with the indicators from that same snapshot. */
export async function settledVerificationState(page, { timeout = 15_000 } = {}) {
  const deadline = performance.now() + timeout;
  for (;;) {
    const { state, waiting, indicators } = await observeVerification(page);
    if (state.pending === 0 && state.flags === 0 && !state.signatureValid) return state;
    if (!waiting || state.signatureValid || performance.now() >= deadline) return { ...state, indicators };
    await new Promise(resolve => setTimeout(resolve, Math.min(100, Math.max(0, deadline - performance.now()))));
  }
}
