#!/usr/bin/env bash
# Builds the Rust core (crates/gwar-app) for Android and generates its Kotlin
# bindings. Gradle runs this before every build; it can also run on its own.
#
# Needs: rustup with the Android targets, cargo-ndk, the Android NDK
# (ANDROID_NDK_HOME or NDK_HOME, else the newest under ANDROID_HOME/ndk), cmake.
#   GWAR_ANDROID_ABIS   space-separated ABIs (default: arm64-v8a)
#   GWAR_RUST_PROFILE   release (default) or dev
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$here/app/build/rust"
abis="${GWAR_ANDROID_ABIS:-arm64-v8a}"
profile="${GWAR_RUST_PROFILE:-release}"
min_sdk=28

# A rustup toolchain, not a system Rust that lacks the Android targets.
if command -v rustup >/dev/null; then
  PATH="$(dirname "$(rustup which cargo)"):$HOME/.cargo/bin:$PATH"
fi
ndk="${ANDROID_NDK_HOME:-${NDK_HOME:-}}"
if [[ -z "$ndk" && -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
  ndk="$ANDROID_HOME/ndk/$(ls "$ANDROID_HOME/ndk" | sort -V | tail -1)"
fi
[[ -n "$ndk" && -d "$ndk" ]] || { echo "Android NDK not found (set ANDROID_NDK_HOME)" >&2; exit 1; }
command -v cargo-ndk >/dev/null || { echo "cargo-ndk missing: cargo install cargo-ndk" >&2; exit 1; }
export ANDROID_NDK_HOME="$ndk" ANDROID_NDK="$ndk"
# Opus (audiopus_sys) is built from source with CMake and linked statically;
# its CMakeLists predates CMake 4.
export OPUS_STATIC=1 CMAKE_POLICY_VERSION_MINIMUM=3.5

targets=()
for abi in $abis; do targets+=(-t "$abi"); done
dir=release
flags=(--release)
if [[ "$profile" == dev ]]; then dir=debug; flags=(); fi

cd "$root"
cargo ndk "${targets[@]}" -P "$min_sdk" -o "$out/jniLibs" build -p gwar-app "${flags[@]}"

first_abi="${abis%% *}"
case "$first_abi" in
  arm64-v8a) triple=aarch64-linux-android ;;
  armeabi-v7a) triple=armv7-linux-androideabi ;;
  x86_64) triple=x86_64-linux-android ;;
  x86) triple=i686-linux-android ;;
  *) echo "unknown ABI $first_abi" >&2; exit 1 ;;
esac
rm -rf "$out/kotlin"
cargo run -q -p gwar-app --features bindgen --bin uniffi-bindgen -- generate \
  --library "target/$triple/$dir/libgwar_app.so" --language kotlin --out-dir "$out/kotlin" --no-format
