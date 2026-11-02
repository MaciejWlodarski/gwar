/** One DOM snapshot: a new pending render cannot race a later flag query. */
export function verificationState(page) {
  return page.evaluate(() => ({
    pending: document.querySelectorAll('[data-message][data-message-verification="pending"]').length,
    flags: document.querySelectorAll('[data-testid^="message-verification-"]').length,
    signatureValid: [...document.querySelectorAll('body *')].some(element =>
      !['SCRIPT', 'STYLE'].includes(element.tagName) && element.textContent?.replace(/\s+/g, ' ').trim() === 'Signature valid'),
  }));
}
