# WebAssembly device agent

CI pins the build tools: Rust 1.99.0, wasm-bindgen-cli 0.2.129 and Binaryen
123.0.0, plus Node **22.23.3** in the `mls-wasm` job. Local builds and size
measurements must use Node **22.23.3** as well: different Node/zlib versions
produce different gzip totals. Verify `node --version` before running these
commands. Install the remaining tools with registry tools:

```sh
pnpm install --frozen-lockfile
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.129 --locked --root .build-tools
npm install --prefix .build-tools --no-audit --no-fund binaryen@123.0.0
WASM_BINDGEN="$PWD/.build-tools/bin/wasm-bindgen" \
WASM_OPT="$PWD/.build-tools/node_modules/.bin/wasm-opt" scripts/mls-wasm/build.sh
node scripts/mls-wasm/smoke.mjs
```

The `wasm-release` profile uses `opt-level=z`, full LTO, one codegen unit,
abort-on-panic and stripped symbols, followed by Binaryen `-Oz --converge`
(repeat optimization passes while binary size decreases). OpenMLS's
`js` feature and getrandom 0.2's `js` feature are enabled. Production wasm
logging is disabled. CI builds this exact target, runs the smoke test, and
fails if all emitted wasm and JavaScript files exceed **480,000 bytes gzip**
(level 9, each file compressed separately). This budget leaves room for
message encryption; load the module on first use and cache it. The round-2
build measured with Rust 1.99.0 and Node 22.23.3 is **450,867 bytes gzip**
(wasm 447,824; minified loader 2,852; inline JavaScript 191), leaving 29,133
bytes of headroom. Generated files in `pkg` are
ignored; Cargo metadata resolves the artifact directory, including custom
`CARGO_TARGET_DIR` values, and a missing artifact fails the build. The loader
is minified with the existing locked frontend Vite/Rolldown dependency; no
new dependency is added. Its package entry is `@gwar/mls-wasm`.

```js
import init, { Agent } from './pkg/gwar_mls_wasm.js';
await init();
const agent = new Agent(seedBytes, credentialBytes, Date.now());
const initialChanges = agent.take_changes();
// Commit initialChanges to the device's four stores before proceeding.
const packages = agent.execute({ key_packages: { count: 5, now: Date.now() } });
const changes = agent.take_changes();
// Commit changes atomically before uploading packages or doing another mutation.
```

The wrapper has four entry points:

- `new Agent(seed: Uint8Array, credential: Uint8Array, now: number)`
- `Agent.load(seed, credential, records)`; `records` has the same shape as
  changes, populated with **all** stored key/value pairs.
- `agent.execute(request)`; synchronous, throws on failure.
- `agent.take_changes()`; returns changes for `openmls`, `groups`, `epochs`,
  and `outbox`. Each is an array of `{key: number[], value: number[] | null}`.

Requests are plain objects with one operation key. Values are the native API's
named arguments, except `id: &[u8; 32]` is named `group`, Welcome uses `id` for
its delivery id, and `key_packages` takes `count`. Byte inputs accept typed
arrays or number arrays; output bytes are number arrays. For example:

```js
agent.execute({ device_hpke_key: null });
// After create, persist, then send mls.group.create. With the group from its
// reply {group} or from conflict, or after a restart from mls.groups (null if
// none): true means ours won; false, join the winner or create again.
agent.execute({ settle: { server, group, winner } });
// Persist before sending, committing, sharing history or joining the winner.
agent.execute({ welcome: { server, id: welcomeId, bytes: welcomeBytes } });
agent.execute({ gate: { server, group, now: Date.now(), revocations: [] } });
agent.execute({ epoch: { server, group, epoch: '18446744073709551615' } });
agent.execute({ reconcile: {
  server, group,
  result: { case: 'entry', epoch: outbox.epoch, commit: outbox.commit, proposals: [] }
} });
```

Operations are `key_packages`, `last_resort_key_package`, `create`, `settle`, `welcome`,
`join_external`, `commit`, `receive`, `proposal`, `pending`, `reconcile`, `gate`,
`sending_key`, `share_history`, `import_history`, `device_hpke_key`, `group`,
`epoch`, and `outbox`. Their native signatures and recovery contract are in
[the core README](../gwar-mls/README.md). `sending_key` returns `[epoch, key]`.
All epoch values are canonical decimal strings, preserving full u64 precision.
Other numeric inputs must be safe integers. Device creation time must be positive;
the HPKE statement returned by `device_hpke_key` preserves that time exactly.
Unknown fields are rejected. `create` leaves `group.status` and `gate.status`
at `creating`, with no sending key, commits or history sharing, until `settle`
activates or deletes it; its changes must be persisted. Eviction deletes all OpenMLS records for that group while retaining
application epoch keys, its pin and last member checkpoint.
Revocations are `{account_key, device_key, revoked_at}` entries, using the
earliest timestamp if repeated. Missing records return `null`.

The caller owns the Web Lock, IndexedDB transactions, connection, current time,
revocation feed and error/retry policy. After any exception or failed storage
transaction, discard/free the agent and load durable state. Call `gate` after
joins/merges to report blocked leaves, and `sending_key` immediately before
sending. There is no UI or delivery service in this wrapper.
