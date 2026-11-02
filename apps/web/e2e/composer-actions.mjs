/** Retry only native input/click actions that never reached the page. An accepted
 * click is attempted once: delivery, failure UI and verification remain the caller's assertions. */
export async function sendComposer(page, text, { timeout = 30_000 } = {}) {
  const deadline = performance.now() + timeout;
  const remaining = () => {
    const ms = Math.ceil(deadline - performance.now());
    if (ms <= 0) throw new Error(`Composer did not accept input and click within ${timeout} ms`);
    return ms;
  };
  const input = page.getByRole('textbox', { name: 'Message', exact: true });
  const send = page.getByRole('button', { name: 'Send message', exact: true });
  const signal = await page.evaluateHandle(() => {
    const state = { typed: false, clicked: false };
    const typed = e => { if (e.target.matches?.('textarea[aria-label="Message"]')) state.typed = true; };
    const clicked = e => { if (e.target.closest?.('button[aria-label="Send message"]:not(:disabled)')) state.clicked = true; };
    document.addEventListener('input', typed, true);
    document.addEventListener('click', clicked, true);
    return { state, close: () => { document.removeEventListener('input', typed, true); document.removeEventListener('click', clicked, true); } };
  });
  try {
    // fill() can report success after the field was disabled during focus,
    // before Chromium inserted text. Retry only if no input event arrived.
    do { await input.fill(text, { timeout: remaining() }); }
    while (!await signal.evaluate(s => s.state.typed));
    if (await input.inputValue() !== text) throw new Error('Composer input was not retained after its input event');
    // A pointerdown can disable Send before the browser dispatches click.
    // Observing the native event avoids retrying an application-level failure.
    do {
      if (await input.inputValue() !== text) throw new Error('Composer input was not retained before its click');
      await send.click({ timeout: remaining() });
    }
    while (!await signal.evaluate(s => s.state.clicked));
  } finally {
    await signal.evaluate(s => s.close()).catch(() => {});
    await signal.dispose();
  }
}
