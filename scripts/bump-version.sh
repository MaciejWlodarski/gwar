#!/usr/bin/env bash
# Sets the release version everywhere scripts/check-version.sh reads it:
#
#   scripts/bump-version.sh 1.2.3
#
# Edits the Cargo manifests, tauri.conf.json and the package.json files, then
# refreshes Cargo.lock. It does not commit, tag or push; it prints what to do
# next. Safe to run again with the same version (it changes nothing).
# Needs bash, awk, sed, jq and cargo.
set -euo pipefail

if [ $# -ne 1 ] || ! [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 X.Y.Z   (for example $0 0.2.0)" >&2
  exit 2
fi
version=$1

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
# shellcheck source=scripts/version-files.sh
source scripts/version-files.sh

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

for f in "${CARGO_MANIFESTS[@]}"; do
  # Only the `version` line under [package]; dependency versions stay.
  awk -v v="$version" '
    /^\[/ { in_package = ($0 == "[package]") }
    in_package && !done && /^version[ \t]*=/ { print "version = \"" v "\""; done = 1; next }
    { print }' "$f" > "$tmp"
  cat "$tmp" > "$f"
done

for f in "${JSON_FILES[@]}"; do
  # Textual, so the file keeps its formatting. Only the top-level key is
  # indented by exactly two spaces.
  sed -E "s/^(  \"version\": *\")[^\"]*(\")/\1${version}\2/" "$f" > "$tmp"
  cat "$tmp" > "$f"
done

# Refresh the workspace members' entries in Cargo.lock. Offline first (nothing
# else changes); fall back to the network.
cargo update --workspace --offline 2>/dev/null || cargo update --workspace

scripts/check-version.sh "v$version"

cat <<NEXT

Version is now $version. Nothing is committed or tagged. Next:

  git diff                                   # review
  git commit -am "Release v$version"
  git tag v$version
  git push origin HEAD v$version

Pushing the tag starts the release workflow; it creates a draft release to
review and publish by hand (see docs/releasing.md).
NEXT
