import { readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';

// Reuse the frontend's locked Vite/Rolldown minifier; no extra dependency.
const webRequire = createRequire(new URL('../../apps/web/package.json', import.meta.url));
const { minify } = await import(pathToFileURL(webRequire.resolve('vite')).href);
const file = process.argv[2];
const result = await minify(file, await readFile(file, 'utf8'), { module: true });
if (result.errors.length) throw new Error(JSON.stringify(result.errors));
await writeFile(file, result.code);
