#!/usr/bin/env python3
"""Exact output keys, computed once before restoring pkg/ (never prefix restores)."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    kind = sys.argv[1]
    if kind not in {"mls", "denoise"}:
        raise ValueError(kind)
    digest = hashlib.sha256()
    # Resolve the transitive local workspace sources without downloading registry dependencies.
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--no-deps", "--locked", "--format-version", "1",
    ]))
    packages = {Path(p["manifest_path"]).parent: p for p in metadata["packages"]}
    pending = [next(root for root, p in packages.items() if p["name"] == f"gwar-{kind}-wasm")]
    roots = set()
    while pending:
        root = pending.pop()
        if root in roots:
            continue
        roots.add(root)
        # Any new excluded local dependency falls back to hashing all tracked crates.
        if root not in packages:
            roots.add(Path.cwd() / "crates")
            continue
        for dependency in packages[root]["dependencies"]:
            if dependency.get("path"):
                pending.append(Path(dependency["path"]))
    source_paths = [str(root.relative_to(Path.cwd())) for root in sorted(roots)]
    # Git's tracked file list includes
    # additions/deletions and excludes generated pkg/ and target/ before AND after the build.
    paths = subprocess.check_output([
        "git", "ls-files", "-z", "Cargo.toml", "Cargo.lock", "rust-toolchain*", ".cargo",
        "**/Cargo.toml", "**/Cargo.lock", "vendor", "scripts", ".github", "package.json",
        "pnpm-lock.yaml", "pnpm-workspace.yaml", *source_paths,
    ]).decode().rstrip("\0").split("\0")
    for path in sorted(paths):
        digest.update(path.encode() + b"\0" + Path(path).read_bytes() + b"\0")
    # Workflow bytes above pin bindgen, Binaryen, Node, Rust, flags, profiles and setup actions.
    # Include actual compiler/Node builds and any externally supplied compilation overrides too.
    for command in (["rustc", "--version", "--verbose"], ["node", "--version"]):
        digest.update(subprocess.check_output(command))
    for key, value in sorted(os.environ.items()):
        if key.startswith(("RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_", "CARGO_PROFILE_",
                           "CARGO_TARGET_WASM32_", "CC_", "CFLAGS_")) or key in {"CC", "CFLAGS"}:
            digest.update(f"{key}={value}\0".encode())
    key = f"wasm-pkg-v1-{os.environ['RUNNER_OS']}-{os.environ['RUNNER_ARCH']}-{kind}-{digest.hexdigest()}"
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        print(f"key={key}", file=output)
    print(key)


if __name__ == "__main__":
    main()
