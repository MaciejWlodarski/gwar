#!/usr/bin/env node
/**
 * Message verification storage in real engines: a whole history verified at once, on fresh and older
 * databases, beside an older tab, across two tabs and a reload, with the revocation feed down and with
 * site storage refused. A result checked while storage or the feed failed is checked again once it
 * works, without a reload; refused storage stays visible.
 *
 *   E2E_BROWSERS=chromium,webkit,firefox   engines to run (default: chromium,webkit)
 */
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium, firefox, webkit } from "playwright";
import { createServer } from "vite";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// A Connect that exists only in this test's routes: nothing leaves the machine.
process.env.VITE_CONNECT_URL = "https://connect.invalid/connect";
const server = await createServer({ root, configFile: path.join(root, "vite.config.ts"), logLevel: "error", server: { port: 0, host: "127.0.0.1" } });
await server.listen();
const base = server.resolvedUrls.local[0];
const engines = { chromium, firefox, webkit };
const names = (process.env.E2E_BROWSERS ?? "chromium,webkit").split(",").map((s) => s.trim()).filter(Boolean);
const N = 300;
const VERIFIED = { verified: N };
const STALE = "Signature valid; revocation information stale or unavailable";
const UNAVAILABLE = `${STALE}; this device cannot use its local storage for message checks right now, so it cannot detect a rollback. The check runs again once storage works; if this persists, the browser may be blocking site data (for example in a private window)`;
let feed = "ok";

async function context(browser, init) {
  const c = await browser.newContext();
  if (init) await c.addInitScript(init);
  await c.route("**/*", (route) => {
    const url = new URL(route.request().url());
    if (url.hostname === "connect.invalid") {
      return feed === "ok" ? route.fulfill({ json: { revocations: [] }, headers: { "access-control-allow-origin": "*" } })
        : route.fulfill({ status: 503, body: "down", headers: { "access-control-allow-origin": "*" } });
    }
    return ["127.0.0.1", "localhost"].includes(url.hostname) ? route.continue() : route.abort();
  });
  return c;
}
async function page(c, messages) {
  const p = await c.newPage();
  await p.goto(`${base}e2e/verification.html`);
  await p.waitForFunction(() => window.fixtureReady);
  if (messages) await p.evaluate((json) => window.fixture.importMessages(json), messages);
  else await p.evaluate((n) => window.fixture.prepare(n), N);
  return p;
}
/** Every message at once; a verification still pending after 20 s is a failure of its own. */
const verify = (p, fresh = true) => p.evaluate((fresh) => Promise.race([window.fixture.verifyAll(fresh),
  new Promise((resolve) => setTimeout(() => resolve("still verifying after 20 s"), 20_000))]), fresh);
const refresh = (p, feed = false) => p.evaluate((feed) => window.fixture.refresh(feed), feed);
const failures = (p) => p.evaluate(() => window.fixture.failures());
/** A tab of the app from before the current schema: version 2, never closing for an upgrade. */
const OLDER = () => new Promise((resolve, reject) => {
  const r = indexedDB.open("gwar-message-auth-v3", 2);
  r.onupgradeneeded = () => { r.result.createObjectStore("checkpoints").createIndex("at", "at"); r.result.createObjectStore("revocations"); r.result.createObjectStore("feeds"); };
  r.onsuccess = () => { window.older = r.result; resolve(); }; r.onerror = () => reject(r.error);
});

const scenarios = {
  async history(browser) {
    const c = await context(browser), p = await page(c);
    assert.deepEqual(await verify(p), VERIFIED);
    assert.deepEqual(await verify(p), VERIFIED, "seen again");
    await c.close();
  },
  async olderDatabase(browser) {
    const c = await context(browser), p = await page(c);
    await p.evaluate(OLDER); await p.evaluate(() => window.older.close());
    assert.deepEqual(await verify(p), VERIFIED);
    assert.deepEqual(await failures(p), { storage: null, feed: null });
    await c.close();
  },
  async olderTab(browser) {
    const c = await context(browser), older = await c.newPage();
    await older.goto(`${base}e2e/verification.html`); await older.evaluate(OLDER);
    const p = await page(c);
    // Unavailable at once, not pending until the other tab closes.
    assert.deepEqual(await verify(p), { [UNAVAILABLE]: N });
    assert.match((await failures(p)).storage, /upgraded in another tab/);
    const recovered = p.evaluate(() => window.fixture.recovered());
    await older.close(); await recovered;
    assert.deepEqual(await refresh(p, true), VERIFIED, "checked again without a reload");
    assert.deepEqual(await failures(p), { storage: null, feed: null });
    await c.close();
  },
  async twoTabs(browser) {
    const c = await context(browser), a = await page(c);
    const b = await page(c, await a.evaluate(() => window.fixture.exportMessages()));
    assert.deepEqual(await Promise.all([verify(a), verify(b)]), [VERIFIED, VERIFIED]);
    await c.close();
  },
  async reload(browser) {
    const c = await context(browser), p = await page(c), messages = await p.evaluate(() => window.fixture.exportMessages());
    void verify(p).catch(() => {});
    await new Promise((resolve) => setTimeout(resolve, 100));
    await p.reload(); await p.waitForFunction(() => window.fixtureReady);
    await p.evaluate((json) => window.fixture.importMessages(json), messages);
    assert.deepEqual(await verify(p), VERIFIED);
    await c.close();
  },
  async feedDown(browser) {
    feed = "down";
    try {
      const c = await context(browser), p = await page(c);
      assert.deepEqual(await verify(p), { [STALE]: N });
      assert.match((await failures(p)).feed, /Revocation feed unavailable/);
      // Back: read again by itself (10 s after the failure), nothing forced and no new messages.
      const recovered = p.evaluate(() => window.fixture.recovered());
      feed = "ok"; await recovered;
      assert.deepEqual(await refresh(p), VERIFIED, "checked again once the feed is back");
      assert.deepEqual(await failures(p), { storage: null, feed: null });
      await c.close();
    } finally { feed = "ok"; }
  },
  async storageRefused(browser) {
    // As a browser refusing site data: opening throws.
    const c = await context(browser, () => { IDBFactory.prototype.open = () => { throw new DOMException("The operation is insecure.", "SecurityError"); }; });
    const p = await page(c);
    assert.deepEqual(await verify(p), { [UNAVAILABLE]: N });
    assert.match((await failures(p)).storage, /SecurityError/);
    assert.deepEqual(await refresh(p, true), { [UNAVAILABLE]: N }, "never upgraded without the check");
    await c.close();
  },
};
const browsers = [];
let passed = 0;
try {
  for (const name of names) {
    const browser = await engines[name].launch();
    browsers.push(browser);
    for (const [scenario, run] of Object.entries(scenarios)) {
      await run(browser);
      passed++;
      console.log(`PASS ${name}: ${scenario}`);
    }
  }
  console.log(`${passed} verification storage checks passed`);
} finally {
  await Promise.all(browsers.map((browser) => browser.close()));
  await server.close();
}
