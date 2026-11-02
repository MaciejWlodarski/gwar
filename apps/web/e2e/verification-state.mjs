/** The original separate reads, retained for the failing-first race regression. */
export async function verificationState(page) {
  return {
    pending: await page.locator('[data-message][data-message-verification="pending"]').count(),
    flags: await page.locator('[data-testid^="message-verification-"]').count(),
    signatureValid: Boolean(await page.getByText('Signature valid', { exact: true }).count()),
  };
}
