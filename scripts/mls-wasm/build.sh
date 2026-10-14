#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

cargo build --locked -p gwar-mls-wasm --target wasm32-unknown-unknown --profile wasm-release
out=crates/gwar-mls-wasm/pkg
"${WASM_BINDGEN:-wasm-bindgen}" --target web --out-dir "$out" target/wasm32-unknown-unknown/wasm-release/gwar_mls_wasm.wasm
"${WASM_OPT:-wasm-opt}" -Oz --enable-bulk-memory --enable-bulk-memory-opt --enable-reference-types --enable-multivalue --enable-sign-ext --enable-nontrapping-float-to-int \
  "$out/gwar_mls_wasm_bg.wasm" -o "$out/gwar_mls_wasm_bg.wasm"
printf '%s\n' '{"name":"@gwar/mls-wasm","private":true,"type":"module","main":"./gwar_mls_wasm.js","types":"./gwar_mls_wasm.d.ts"}' > "$out/package.json"
node scripts/mls-wasm/size.mjs "$out"
