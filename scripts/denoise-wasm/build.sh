#!/bin/sh
# Builds the web app's noise suppression into crates/gwar-denoise-wasm/pkg: the wasm module, and
# the model's archive (pinned in crates/gwar-denoise-wasm/model.json) from the libDF checkout
# cargo uses, the one the desktop app embeds.
set -eu
cd "$(dirname "$0")/../.."

# SIMD kernels; getrandom gets the crate's refusing backend, so the module imports nothing.
RUSTFLAGS='-C target-feature=+simd128 --cfg getrandom_backend="custom"' \
  cargo build --locked -p gwar-denoise-wasm --target wasm32-unknown-unknown --profile wasm-fast
metadata=$(cargo metadata --format-version 1)
target_dir=$(printf '%s' "$metadata" | node -e 'let d = ""; process.stdin.on("data", c => d += c); process.stdin.on("end", () => console.log(JSON.parse(d).target_directory));')
libdf=$(printf '%s' "$metadata" | node -e 'let d = ""; process.stdin.on("data", c => d += c); process.stdin.on("end", () => console.log(JSON.parse(d).packages.find(p => p.name === "deep_filter").manifest_path));')
artifact="$target_dir/wasm32-unknown-unknown/wasm-fast/gwar_denoise_wasm.wasm"
if [ ! -f "$artifact" ]; then
  printf 'Missing wasm artifact: %s\n' "$artifact" >&2
  exit 1
fi
out=crates/gwar-denoise-wasm/pkg
rm -rf "$out"
mkdir -p "$out"
"${WASM_OPT:-wasm-opt}" -O3 --enable-simd --enable-bulk-memory --enable-bulk-memory-opt --enable-reference-types --enable-multivalue --enable-sign-ext --enable-nontrapping-float-to-int \
  "$artifact" -o "$out/gwar_denoise.wasm"
model=$(node -e 'console.log(require("./crates/gwar-denoise-wasm/model.json").file)')
# Not as .gz: some servers (Vite's preview among them) would send it gzip-encoded, and the
# browser would hand the page the tar instead.
cp "$(dirname "$libdf")/../models/$model" "$out/model.bin"
node scripts/denoise-wasm/check.mjs "$out"
