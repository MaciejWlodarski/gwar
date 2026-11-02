import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { chromium } from 'playwright';
import { sendComposer } from './composer-actions.mjs';
let browser;
before(async () => { browser = await chromium.launch({ args: ['--disable-audio-output'] }); });
after(async () => { await browser?.close(); });
async function fixture(fn) {
  const page = await browser.newPage();
  try {
    await page.setContent(`<textarea aria-label="Message"></textarea><button aria-label="Send message" disabled>Send</button><script>
      window.sends = 0;
      document.querySelector('textarea').oninput = e => document.querySelector('button').disabled = !e.target.value.trim();
      document.querySelector('button').onclick = () => { window.sends++; document.querySelector('textarea').value = ''; };
    </script>`);
    await fn(page);
  } finally { await page.close(); }
}
function withLocators(page, input, send) {
  return {
    evaluateHandle: page.evaluateHandle.bind(page),
    getByRole: role => role === 'textbox' ? input : send,
  };
}
test('re-enters text only when disabling during focus prevented the native input event', async () => fixture(async page => {
  const input = page.getByRole('textbox'), send = page.getByRole('button');
  await input.evaluate(el => el.addEventListener('focus', () => { el.disabled = true; }, { once: true }));
  const fill = input.fill.bind(input);
  let attempts = 0;
  input.fill = async (...args) => {
    await fill(...args); attempts++;
    // Release only after the insertion has completed: no wall-clock race in the reproducer.
    await input.evaluate(el => { el.disabled = false; });
  };
  await sendComposer(withLocators(page, input, send), 'psst', { timeout: 1000 });
  assert.equal(attempts, 2);
  assert.equal(await page.evaluate(() => window.sends), 1);
}));
test('repeats a physical click suppressed by disabling between pointerdown and click', async () => fixture(async page => {
  const input = page.getByRole('textbox'), send = page.getByRole('button');
  await send.evaluate(el => el.addEventListener('pointerdown', () => { el.disabled = true; }, { once: true }));
  const click = send.click.bind(send);
  let attempts = 0;
  send.click = async (...args) => {
    await click(...args); attempts++;
    await send.evaluate(el => { el.disabled = false; });
  };
  await sendComposer(withLocators(page, input, send), 'psst', { timeout: 1000 });
  assert.equal(attempts, 2);
  assert.equal(await page.evaluate(() => window.sends), 1);
}));
test('does not retry delivery after an accepted click leaves the draft in place', async () => fixture(async page => {
  await page.getByRole('button').evaluate(el => { el.onclick = () => { window.sends++; }; });
  await sendComposer(page, 'psst', { timeout: 1000 });
  assert.equal(await page.evaluate(() => window.sends), 1);
  assert.equal(await page.getByRole('textbox').inputValue(), 'psst');
}));
test('fails instead of retyping when the app loses text after accepting its input event', async () => fixture(async page => {
  await page.getByRole('textbox').evaluate(el => { el.oninput = () => { el.value = ''; }; });
  await assert.rejects(sendComposer(page, 'psst', { timeout: 1000 }), /not retained/);
  assert.equal(await page.evaluate(() => window.sends), 0);
}));
