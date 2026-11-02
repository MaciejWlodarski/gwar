"""Hosted-only call timing; the production model and every quality/task budget are unchanged."""
from pathlib import Path

p = Path('apps/web/src/voice/webrtc-engine.ts')
s = p.read_text()
s = '''const _callMark = (stage: string) => (window as unknown as { __gwarDenoiseCallMark?: (stage: string) => void }).__gwarDenoiseCallMark?.(stage);
''' + s
s = s.replace('  private loadDenoise(): void {', '  private loadDenoise(): void {\n    _callMark("load-start");', 1)
s = s.replace('        this.denoise = loaded;', '        this.denoise = loaded;\n        _callMark("model-ready");', 1)
s = s.replace('      this.denoise?.setSource(stream);', '      this.denoise?.setSource(stream);\n      if (this.denoise) _callMark("source-ready");', 1)
assert s.count('_callMark("source-ready")') == 1
p.write_text(s)

registry = Path('apps/web/src/e2e.ts')
s = registry.read_text()
entry = '  "/src/voice/webrtc-engine.ts": () => import("./voice/webrtc-engine.ts"),'
if entry not in s:
    s = s.replace('const modules: Record<string, () => Promise<unknown>> = {', 'const modules: Record<string, () => Promise<unknown>> = {\n' + entry, 1)
registry.write_text(s)

common = Path('apps/web/e2e/denoise.mjs').read_text()
start = common.index('async function measure() {')
end = common.index('\nasync function run(', start)
probe = common[:start] + Path('scripts/ci/denoise-idle-measure.js').read_text() + common[end:]
probe = probe.replace('"--autoplay-policy=no-user-gesture-required",', '"--autoplay-policy=no-user-gesture-required", "--use-fake-ui-for-media-stream", "--use-fake-device-for-media-stream",', 1)
probe = probe.replace('"media.autoplay.default": 0,', '"media.navigator.streams.fake": true, "media.navigator.permission.disabled": true, "media.autoplay.default": 0,', 1)
probe = probe.replace('const tracing = engine === "chromium";', 'const tracing = false;', 1)
probe = probe.replace('    const page = await browser.newPage();', '''    const page = await browser.newPage();
    await page.exposeFunction("__denoiseJoinGesture", () => page.mouse.click(1, 1));
    if (engine !== "firefox") await page.context().grantPermissions(["microphone"], { origin: web });
    await page.route("**/*", route => ["127.0.0.1", "localhost"].includes(new URL(route.request().url()).hostname) ? route.continue() : route.abort());
    await page.addInitScript(() => {
      window.__denoiseWorkers = { created: 0, active: new Set() };
      window.Worker = new Proxy(window.Worker, {
        construct(target, args) {
          const worker = new target(...args);
          if (args[1]?.name?.startsWith("gwar-denoise")) {
            window.__denoiseWorkers.created++;
            window.__denoiseWorkers.active.add(worker);
            const terminate = worker.terminate.bind(worker);
            worker.terminate = () => { window.__denoiseWorkers.active.delete(worker); terminate(); };
          }
          return worker;
        },
      });
    });''', 1)
probe = probe.replace('    const result = await Promise.race([', '    await page.evaluate(scenario => { window.__denoiseCase = scenario; }, process.env.E2E_DENOISE_CASE);\n    const result = await Promise.race([', 1)
start = probe.index('    if (tracing) {')
end = probe.index('    results.push({ engine, status: "pass", ...result, errors });', start)
probe = probe[:start] + '    assert.deepEqual(result.failures, [], "call startup failed");\n' + probe[end:]
Path('apps/web/e2e/denoise-join.mjs').write_text(probe)
