#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."

cargo build --locked -p gwar-mls-wasm --target wasm32-unknown-unknown --profile wasm-release
target_dir=$(cargo metadata --format-version 1 --no-deps | node -e 'let data = ""; process.stdin.on("data", chunk => data += chunk); process.stdin.on("end", () => console.log(JSON.parse(data).target_directory));')
artifact="$target_dir/wasm32-unknown-unknown/wasm-release/gwar_mls_wasm.wasm"
if [ ! -f "$artifact" ]; then
  printf 'Missing wasm artifact: %s\n' "$artifact" >&2
  exit 1
fi
out=crates/gwar-mls-wasm/pkg
"${WASM_BINDGEN:-wasm-bindgen}" --target web --out-dir "$out" "$artifact"
"${WASM_OPT:-wasm-opt}" -Oz --converge --enable-bulk-memory --enable-bulk-memory-opt --enable-reference-types --enable-multivalue --enable-sign-ext --enable-nontrapping-float-to-int \
  "$out/gwar_mls_wasm_bg.wasm" -o "$out/gwar_mls_wasm_bg.wasm"
printf '%s\n' '{"name":"@gwar/mls-wasm","private":true,"type":"module","main":"./gwar_mls_wasm.js","types":"./gwar_mls_wasm.d.ts"}' > "$out/package.json"
node scripts/mls-wasm/minify.mjs "$out/gwar_mls_wasm.js"
node scripts/mls-wasm/size.mjs "$out"
