/** The original smoke action, extracted unchanged for failing-first browser regressions. */
export async function sendComposer(page, text, { timeout = 30_000 } = {}) {
  await page.getByRole('textbox', { name: 'Message', exact: true }).fill(text, { timeout });
  await page.getByRole('button', { name: 'Send message', exact: true }).click({ timeout });
}
