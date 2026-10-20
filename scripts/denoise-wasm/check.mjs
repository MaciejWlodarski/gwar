// Checks a built crates/gwar-denoise-wasm/pkg: the model is the pinned one, the module imports
// nothing (it runs on the browser's audio thread), and it cleans noise off a tone in real time.
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';

const directory = process.argv[2] ?? 'crates/gwar-denoise-wasm/pkg';
const pin = JSON.parse(await readFile(new URL('../../crates/gwar-denoise-wasm/model.json', import.meta.url), 'utf8'));
const archive = await readFile(join(directory, 'model.bin'));
const sha256 = createHash('sha256').update(archive).digest('hex');
if (sha256 !== pin.sha256) throw new Error(`${pin.file}: sha256 ${sha256}, pinned ${pin.sha256}`);

const bytes = await readFile(join(directory, 'gwar_denoise.wasm'));
console.log(`gwar_denoise.wasm: ${bytes.length} bytes, ${gzipSync(bytes, { level: 9 }).length} bytes gzip; model.bin (${pin.file}): ${archive.length} bytes`);
const module = await WebAssembly.compile(bytes);
const imports = WebAssembly.Module.imports(module);
if (imports.length) throw new Error(`the module imports ${imports.map(i => `${i.module}.${i.name}`).join(', ')}`);
const { exports } = await WebAssembly.instantiate(module, {});
const memory = () => exports.memory.buffer;

const at = exports.gwar_denoise_archive(archive.length);
new Uint8Array(memory(), at, archive.length).set(archive);
const denoiser = exports.gwar_denoise_new();
if (!denoiser) throw new Error(new TextDecoder().decode(new Uint8Array(memory(), exports.gwar_denoise_error(), exports.gwar_denoise_error_len())));
const length = exports.gwar_denoise_frame_length();
if (length !== 480) throw new Error(`frame length ${length}`);

// Steady white noise at -30 dBFS for 3 s: past the first second, it must be far down.
let seed = 1;
const noise = () => { seed = (seed * 1664525 + 1013904223) >>> 0; return (seed / 2 ** 32 - 0.5) * 2 * Math.sqrt(3) * 10 ** (-30 / 20); };
let inEnergy = 0, outEnergy = 0, slowest = 0, total = 0;
const frames = 300, settled = 100;
for (let f = 0; f < frames; f++) {
  const frame = new Float32Array(memory(), exports.gwar_denoise_frame(denoiser), length);
  for (let i = 0; i < length; i++) frame[i] = noise();
  if (f >= settled) for (const s of frame) inEnergy += s * s;
  const start = performance.now();
  if (!exports.gwar_denoise_process(denoiser, 100)) throw new Error('process failed');
  const took = performance.now() - start;
  if (f >= settled) {
    slowest = Math.max(slowest, took);
    total += took;
    for (const s of new Float32Array(memory(), exports.gwar_denoise_frame(denoiser), length)) outEnergy += s * s;
  }
}
const cut = 10 * Math.log10(inEnergy / outEnergy), mean = total / (frames - settled);
console.log(`noise down ${cut.toFixed(1)} dB; ${mean.toFixed(2)} ms a frame, slowest ${slowest.toFixed(2)} ms`);
if (!(cut > 20)) throw new Error('the model did not take the noise down');
if (mean > 10) throw new Error('slower than real time');
