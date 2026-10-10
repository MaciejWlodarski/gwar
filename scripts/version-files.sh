# shellcheck shell=bash disable=SC2034
# Where the release version lives. Sourced by check-version.sh and
# bump-version.sh, so both always look at the same files.
#
# Every crate and package carries its own version (there is no
# [workspace.package] version), and they are released together.

# Cargo manifests: the `version` under [package].
CARGO_MANIFESTS=(
  crates/vc-server/Cargo.toml
  crates/gwar-connect/Cargo.toml
  crates/vc-client/Cargo.toml
  crates/vc-proto/Cargo.toml
  apps/desktop/src-tauri/Cargo.toml
)

# JSON files with a top-level "version" key (indented by two spaces).
# The desktop app's version comes from tauri.conf.json.
JSON_FILES=(
  apps/desktop/src-tauri/tauri.conf.json
  apps/desktop/package.json
  apps/web/package.json
)
