import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { verificationState } from './verification-state.mjs';
let browser;
before(async () => { browser = await chromium.launch({ args: ['--disable-audio-output'] }); });
after(async () => { await browser?.close(); });
const pending = '<div data-message="2" data-message-verification="pending"><span data-testid="message-verification-pending">Checking the signature…</span></div>';
test('a new pending render cannot be mistaken for a settled verification warning', async () => {
  const page = await browser.newPage();
  try {
    await page.setContent('<main>Verified messages</main>');
    const locator = page.locator.bind(page);
    const alternating = {
      evaluate: page.evaluate.bind(page), getByText: page.getByText.bind(page),
      locator: selector => {
        const result = locator(selector);
        if (selector.includes('data-message-verification="pending"')) {
          const count = result.count.bind(result);
          result.count = async () => {
            const before = await count();
            // The actual loaded-CI sequence: no pending row at the first read,
            // then a pending indicator before the subsequent flag read.
            await page.locator('main').evaluate((el, html) => { el.innerHTML = html; }, pending);
            return before;
          };
        }
        return result;
      },
    };
    const state = await verificationState(alternating);
    assert.ok(state.pending > 0 || state.flags === 0, 'pending must not be reported as a settled warning');
  } finally { await page.close(); }
});
test('retains pending and real invalid indicators in their actual states', async () => {
  const page = await browser.newPage();
  try {
    await page.setContent(pending);
    assert.deepEqual(await verificationState(page), { pending: 1, flags: 1, signatureValid: false });
    await page.setContent('<div data-message="2" data-message-verification="invalid"><span data-testid="message-verification-invalid">Invalid signature</span></div>');
    assert.deepEqual(await verificationState(page), { pending: 0, flags: 1, signatureValid: false });
    await page.setContent('<span>Signature valid</span>');
    assert.deepEqual(await verificationState(page), { pending: 0, flags: 0, signatureValid: true });
  } finally { await page.close(); }
});
