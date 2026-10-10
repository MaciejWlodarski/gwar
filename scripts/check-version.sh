#!/usr/bin/env bash
# Checks that the version is the same everywhere it is written down.
#
#   scripts/check-version.sh v1.2.3   # also require it to equal the tag
#   scripts/check-version.sh          # only require all places to agree
#
# The release workflow calls it with the pushed tag. Exits 1 on any mismatch.
# Needs bash, awk and jq.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
# shellcheck source=scripts/version-files.sh
source scripts/version-files.sh

# `version` under [package] in a Cargo manifest.
cargo_version() {
  awk '
    /^\[/ { in_package = ($0 == "[package]") }
    in_package && /^version[ \t]*=/ {
      sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
    }' "$1"
}

# Version of a workspace package in Cargo.lock.
lock_version() {
  awk -v want="$1" '
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^version = / && name == want { v = $3; gsub(/"/, "", v); print v; exit }' Cargo.lock
}

package_name() {
  awk '/^\[/ { p = ($0 == "[package]") } p && /^name[ \t]*=/ { sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit }' "$1"
}

expected=""
if [ $# -gt 1 ]; then
  echo "usage: $0 [vX.Y.Z]" >&2
  exit 2
elif [ $# -eq 1 ]; then
  expected=${1#refs/tags/}
  expected=${expected#v}
  if ! [[ $expected =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: '$1' is not a release version (expected vX.Y.Z, for example v1.2.3)" >&2
    exit 2
  fi
fi

fail=0
found=()
check() { # label, version
  local label=$1 got=$2
  if [ -z "$got" ]; then
    printf '  MISSING  %s\n' "$label"
    fail=1
    return
  fi
  found+=("$got")
  # Without a tag, the first place found is the reference.
  [ -n "$expected" ] || expected=$got
  if [ "$got" = "$expected" ]; then
    printf '  ok       %-48s %s\n' "$label" "$got"
  else
    printf '  MISMATCH %-48s %s (expected %s)\n' "$label" "$got" "$expected"
    fail=1
  fi
}

for f in "${CARGO_MANIFESTS[@]}"; do
  check "$f" "$(cargo_version "$f")"
done
for f in "${JSON_FILES[@]}"; do
  check "$f" "$(jq -r '.version // empty' "$f")"
done
# Cargo.lock must already record the same versions, or `cargo --locked` fails.
for f in "${CARGO_MANIFESTS[@]}"; do
  name=$(package_name "$f")
  check "Cargo.lock ($name)" "$(lock_version "$name")"
done

if [ "$fail" -ne 0 ]; then
  echo "error: version mismatch. Run scripts/bump-version.sh X.Y.Z to fix it." >&2
  exit 1
fi
echo "All ${#found[@]} version entries are $expected."
# Under GitHub Actions, hand the version to later jobs.
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "version=$expected" >> "$GITHUB_OUTPUT"
fi
