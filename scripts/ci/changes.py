#!/usr/bin/env python3
"""Fail closed: only documentation and web-only diffs can reduce CI coverage."""
import os
from pathlib import PurePosixPath
import subprocess


def select(paths, full=False):
    native = app = full
    for path in paths:
        # Shared inputs win over the documentation/web exceptions below.
        # gwar-media runs in the native Rust suite and on every desktop OS.
        if path.startswith((".github/", "scripts/", "apps/web/src/proto/", "crates/gwar-media/")):
            return {"native": True, "app": True}
        name = PurePosixPath(path).name
        if "lock" in name or name in {"Cargo.toml", "package.json"}:
            return {"native": True, "app": True}
        if (path.startswith("docs/") and PurePosixPath(path).suffix in
                {".md", ".png", ".jpg", ".jpeg", ".webp", ".gif"}) or (
                name in {"README.md", "CHANGELOG.md", "CONTRIBUTING.md", "AGENTS.md"}):
            continue
        if path.startswith("apps/web/"):
            # The same frontend is embedded in desktop and exercises every browser suite.
            app = True
        else:
            return {"native": True, "app": True}
    return {"native": native, "app": app}


def main():
    full = os.environ.get("GITHUB_EVENT_NAME") != "pull_request"
    paths = []
    if not full:
        try:
            # No rename detection: both the old and new paths must affect selection.
            paths = subprocess.check_output([
                "git", "diff", "--no-renames", "--name-only", "-z",
                f"{os.environ['BASE_SHA']}...{os.environ['HEAD_SHA']}", "--",
            ]).decode().rstrip("\0").split("\0")
            paths = [p for p in paths if p]
        except (KeyError, subprocess.CalledProcessError, UnicodeError) as error:
            print(f"Cannot determine changed files; running all coverage: {error}")
            full = True
    selected = select(paths, full)
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        for name, value in selected.items():
            print(f"{name}={str(value).lower()}", file=output)
    print(f"Changed files: {paths}; selected: {selected}; full: {full}")


if __name__ == "__main__":
    main()
