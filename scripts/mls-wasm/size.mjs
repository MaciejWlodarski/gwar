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
console.log(`Total gzip: ${total} / 450000 bytes`);
if (total > 450_000) process.exitCode = 1;
