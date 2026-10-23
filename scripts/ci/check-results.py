#!/usr/bin/env python3
"""The single branch-protection gate; skipped required jobs are failures too."""
import json
import os


def check(needs):
    if needs["changes"]["result"] != "success":
        raise ValueError("changed-file selection did not succeed")
    flags = needs["changes"]["outputs"]
    if any(flags.get(name) not in {"true", "false"} for name in ("native", "app")):
        raise ValueError("missing/invalid changed-file outputs")
    native = {"rust", "rust-voice", "rust-screen-sim", "teamspeak"}
    app = {"audit", "web", "desktop", "mls-wasm", "denoise-wasm", "native-build",
           "browser-mls", "browser-smoke", "browser-voice", "browser-screen", "browser-private"}
    # An added/renamed job must be wired here and into ci-ok.needs deliberately.
    if set(needs) != {"changes"} | native | app:
        raise ValueError("unexpected aggregate dependencies")
    for name in sorted(native | app):
        required = flags["native" if name in native else "app"] == "true"
        expected = "success" if required else "skipped"
        actual = needs[name]["result"]
        print(f"{name}: {actual} (expected {expected})")
        if actual != expected:
            raise ValueError(f"{name}: {actual}, expected {expected}")


if __name__ == "__main__":
    check(json.loads(os.environ["NEEDS"]))
