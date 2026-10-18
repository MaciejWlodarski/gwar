import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';

const directory = process.argv[2] ?? 'crates/gwar-mls-wasm/pkg';
const files = (await readdir(directory, { recursive: true }))
  .filter(path => /\.(wasm|js)$/.test(path)).sort();
if (!files.some(path => path.endsWith('.wasm'))) throw new Error('No wasm artifact');
let total = 0;
for (const file of files) {
  const bytes = await readFile(join(directory, file));
  const gzip = gzipSync(bytes, { level: 9 }).length;
  console.log(`${file}: ${bytes.length} bytes, ${gzip} bytes gzip`);
  total += gzip;
}
// docs/e2e.md, "The MLS library": the budget and why it was raised.
const budget = 620_000;
console.log(`Total gzip: ${total} / ${budget} bytes`);
if (total > budget) process.exitCode = 1;
