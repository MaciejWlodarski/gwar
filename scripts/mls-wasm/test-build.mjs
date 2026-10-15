import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import { gzipSync } from 'node:zlib';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const root = await mkdtemp(join(tmpdir(), 'gwar-wasm-build-'));
try {
  const bin = join(root, 'bin');
  const target = join(root, 'custom target');
  const artifact = join(target, 'wasm32-unknown-unknown/wasm-release/gwar_mls_wasm.wasm');
  await mkdir(bin);
  await mkdir(join(target, 'wasm32-unknown-unknown/wasm-release'), { recursive: true });
  await writeFile(join(bin, 'cargo'), `#!/bin/sh
case "$1" in
  build) exit 0 ;;
  metadata) printf '%s\\n' "$TEST_METADATA" ;;
  *) exit 99 ;;
esac
`, { mode: 0o755 });
  // Stop before writing generated bindings; only the exact metadata artifact may be used.
  await writeFile(join(bin, 'wasm-bindgen'), `#!/bin/sh
for arg do last="$arg"; done
[ "$last" = "$TEST_ARTIFACT" ] || exit 99
exit 23
`, { mode: 0o755 });
  const run = () => spawnSync('sh', [resolve('scripts/mls-wasm/build.sh')], {
    encoding: 'utf8',
    env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, CARGO_TARGET_DIR: target,
      TEST_METADATA: JSON.stringify({ target_directory: target }), TEST_ARTIFACT: artifact,
      WASM_BINDGEN: join(bin, 'wasm-bindgen') },
  });
  await writeFile(artifact, 'test artifact');
  let result = run();
  assert.equal(result.status, 23, result.stderr);
  await rm(artifact);
  result = run();
  assert.equal(result.status, 1, result.stderr);
  assert.ok(result.stderr.includes(`Missing wasm artifact: ${artifact}`), result.stderr);
  console.log('Build path checks passed: custom target with spaces; missing artifact fails without fallback.');

  const pkg = join(root, 'pkg');
  await mkdir(join(pkg, 'snippets'), { recursive: true });
  const wasm = join(pkg, 'agent.wasm');
  const inline = join(pkg, 'snippets/inline.js');
  const size = () => spawnSync(process.execPath, [resolve('scripts/mls-wasm/size.mjs'), pkg], { encoding: 'utf8' });
  // Incompressible bytes exercise the new headroom and aggregate nested JS.
  const payload = randomBytes(490_000);
  await writeFile(wasm, payload);
  result = size();
  assert.equal(result.status, 0, result.stdout + result.stderr);
  assert.ok(result.stdout.includes(`Total gzip: ${gzipSync(payload, { level: 9 }).length} / 500000 bytes`));
  await writeFile(inline, randomBytes(20_000));
  result = size();
  assert.equal(result.status, 1, result.stdout + result.stderr);
  await rm(wasm);
  result = size();
  assert.notEqual(result.status, 0);
  assert.ok(result.stderr.includes('No wasm artifact'), result.stderr);
  console.log('Size checks passed: 490 KB accepted; aggregate wasm + nested JS over 500 KB rejected; missing wasm rejected.');
} finally {
  await rm(root, { recursive: true, force: true });
}
