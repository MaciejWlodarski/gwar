"""Hosted-only phase marks for suppression readiness; behavior, model and budgets are unchanged.

Run after denoise-idle-instrument.py. Every thread stamps Date.now(); the page's call mark turns
that into milliseconds since the measured join started. Worker and worklet marks ride the
existing ports as `{type: "mark"}` messages that production code never sends.
"""
from pathlib import Path

root = Path('apps/web/src/voice/denoise')
PAGE = 'const _dmark = (stage: string, at?: number) => (window as unknown as { __gwarDenoiseCallMark?: (stage: string, at?: number) => void }).__gwarDenoiseCallMark?.(stage, at);\n'


def patch(name, edits, prefix=PAGE):
    p = root / name
    s = p.read_text()
    for old, new in edits:
        assert s.count(old) == 1, (name, old)
        s = s.replace(old, new, 1)
    p.write_text(prefix + s)


forward = '      if ((report as unknown as { type: string }).type === "mark") { const m = report as unknown as { stage: string; at: number }; _dmark(m.stage, m.at); return; }\n'

if (root / 'build-worker.ts').exists():
    patch('pipeline-pool.ts', [
        ('    signal.throwIfAborted();\n    if (!this.enabled || this.failure)', '    _dmark(this.spare ? "acquire-spare" : "acquire-new");\n    signal.throwIfAborted();\n    if (!this.enabled || this.failure)'),
    ])
    patch('model-cache.ts', [
        ('      const assets = await this.host.assets();\n', '      const assets = await this.host.assets();\n      _dmark("assets");\n'),
        ('      entry.worker = worker;\n', '      entry.worker = worker;\n      _dmark("worker");\n'),
        ('        if (!entry.active) return;\n        if (event.data.type === "ready") {', '        if (!entry.active) return;\n        if ((event.data as unknown as { type: string }).type === "mark") { const m = event.data as unknown as { stage: string; at: number }; _dmark(`worker:${m.stage}`, m.at); return; }\n        if (event.data.type === "ready") {'),
    ])
    patch('index.ts', [
        ('    context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });\n', '    _dmark("connected");\n    context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });\n    _dmark("context");\n'),
        ('    await context.audioWorklet.addModule(model.worklet);\n', '    await context.audioWorklet.addModule(model.worklet);\n    _dmark("module");\n'),
        ('    const receive = (report: Report) => {\n', '    const receive = (report: Report) => {\n' + forward + '      if (report.type === "loaded" || report.type === "cleared" || report.type === "awake") _dmark(`page:${report.type}`);\n'),
        ('    const report = await ready;\n', '    const report = await ready;\n    _dmark(`page:${report.type}`);\n'),
        ('        idle = false;\n        const token = ++wakeToken;', '        _dmark("wake");\n        idle = false;\n        const token = ++wakeToken;'),
        ('          if (waking === pending) post({ type: "wake", token });', '          _dmark("resumed");\n          if (waking === pending) post({ type: "wake", token });'),
        ('        if (parked) return parked;\n', '        if (parked) return parked;\n        _dmark("park");\n'),
        ('          await context.suspend();\n', '          await context.suspend();\n          _dmark("suspended");\n'),
    ])
    patch('build-worker.ts', [
        ('async function build({', 'const _mark = (stage: string) => (globalThis as unknown as { postMessage(m: unknown): void }).postMessage({ type: "mark", stage, at: Date.now() });\nasync function build({'),
        ('  code = await asset(cache, codeUrl);\n', '  _mark("start");\n  code = await asset(cache, codeUrl);\n  _mark("code");\n'),
        ('  const module = await WebAssembly.compile(code);\n', '  const module = await WebAssembly.compile(code);\n  _mark("compiled");\n'),
        ('  const bytes = new Uint8Array(await archive);\n', '  const bytes = new Uint8Array(await archive);\n  _mark("archive");\n'),
        ('  model = { exports, size', '  _mark("built");\n  model = { exports, size'),
        ('    if (last) return detach(id);\n', '    if (last) { _mark("sent"); return detach(id); }\n'),
    ], prefix='')
    patch('worklet.ts', [
        ('  private receive(request: Request): void {\n', '  private mark(stage: string): void { this.port.postMessage({ type: "mark", stage: `worklet:${stage}`, at: Date.now() }); }\n  private fast = 0;\n\n  private receive(request: Request): void {\n'),
        ('      case "code":\n', '      case "code":\n        this.mark("code");\n'),
        ('            this.loading = instance.exports as unknown as Exports;\n', '            this.loading = instance.exports as unknown as Exports;\n            this.mark("instantiated");\n'),
        ('        growMemory(this.loading!.memory, message.size);\n', '        this.mark("model");\n        growMemory(this.loading!.memory, message.size);\n'),
        ('        this.worker!.close();\n        this.worker = null;\n        this.loaded(this.loading!);', '        this.mark("copied");\n        this.worker!.close();\n        this.worker = null;\n        this.loaded(this.loading!);'),
        ('    const frame = this.noise();\n    const started = Date.now();\n    if (!this.run(frame, false)) return;\n', '    if (this.warming === WARM_UP) this.mark("warm-start");\n    const frame = this.noise();\n    const started = Date.now();\n    if (!this.run(frame, false)) return;\n'),
        ('    const mean = this.warmUpMs / (WARM_UP / 2);\n', '    const mean = this.warmUpMs / (WARM_UP / 2);\n    this.mark(`warm-done fast=${this.fast} mean=${mean.toFixed(2)} cpu=${this.warmCpuMs}`);\n'),
        ('(this.quanta++ % 2 === 0 || fast)) this.warmUp();', '(this.quanta++ % 2 === 0 || fast)) { if (this.quanta % 2 === 0) this.fast++; this.warmUp(); }'),
    ], prefix='')
else:
    # The main control: one load message, the model built on the audio thread.
    patch('index.ts', [
        ('  const { code, archive, worklet } = await assets(signal);\n', '  const { code, archive, worklet } = await assets(signal);\n  _dmark("assets");\n'),
        ('  const context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });\n', '  const context = new AudioContext({ sampleRate: 48_000, latencyHint: "interactive" });\n  _dmark("context");\n'),
        ('    await context.audioWorklet.addModule(worklet);\n', '    await context.audioWorklet.addModule(worklet);\n    _dmark("module");\n'),
        ('    const receive = (report: Report) => {\n', '    const receive = (report: Report) => {\n' + forward),
        ('    const report = await ready;\n', '    const report = await ready;\n    _dmark(`page:${report.type}`);\n'),
    ])
    patch('worklet.ts', [
        ('  private receive(request: Request): void {\n', '  private mark(stage: string): void { this.port.postMessage({ type: "mark", stage: `worklet:${stage}`, at: Date.now() }); }\n\n  private receive(request: Request): void {\n'),
        ('      case "load":\n', '      case "load":\n        this.mark("load");\n'),
        ('          .then(module => WebAssembly.instantiate(module, {}))\n          .then(instance => this.load(', '          .then(module => WebAssembly.instantiate(module, {}))\n          .then(instance => { this.mark("instantiated"); return instance; })\n          .then(instance => this.load('),
        ('    this.exports = exports;\n    this.denoiser = denoiser;\n', '    this.exports = exports;\n    this.denoiser = denoiser;\n    this.mark("built");\n'),
        ('    const started = Date.now();\n    if (!this.run(frame, false)) return;\n', '    if (this.warming === WARM_UP) this.mark("warm-start");\n    const started = Date.now();\n    if (!this.run(frame, false)) return;\n'),
        ('    const mean = this.warmUpMs / (WARM_UP / 2);\n', '    const mean = this.warmUpMs / (WARM_UP / 2);\n    this.mark(`warm-done mean=${mean.toFixed(2)}`);\n'),
    ], prefix='')
