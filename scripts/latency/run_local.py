#!/usr/bin/env python3
"""Compare native latency binaries with fresh loopback servers, in balanced order."""

import argparse
import csv
import hashlib
import json
import math
import os
import platform
import re
import signal
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
QUANTILES = (("median_ms", 0.5), ("p95_ms", 0.95), ("p99_ms", 0.99))


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def variant(value):
    name, separator, binary = value.partition("=")
    if not separator or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]*", name):
        raise argparse.ArgumentTypeError("expected name=path/to/binary; name must use letters, digits, _ or -")
    path = Path(binary).expanduser().resolve()
    if not path.is_file() or not os.access(path, os.X_OK):
        raise argparse.ArgumentTypeError(f"not an executable file: {path}")
    return name, path


def add_arguments(parser):
    parser.add_argument("--variant", type=variant, action="append", required=True, help="name=path/to/client-binary; repeatable")
    parser.add_argument("--markers", type=positive, default=100, help="markers per run (default: 100)")
    parser.add_argument("--runs", type=positive, default=4, help="runs per variant (default: 4); use an even number for balance")
    parser.add_argument("--out", type=Path, required=True, help="new output directory; never overwritten")


def cancelled(signum, frame):
    raise KeyboardInterrupt(f"signal {signum}")


def install_signals():
    signal.signal(signal.SIGTERM, cancelled)


def environment():
    return {key: value for key, value in os.environ.items() if not key.startswith(("VC_", "LATENCY_"))}


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run_client(binary, csv_path, markers, env):
    log_path = csv_path.with_suffix(".txt")
    with log_path.open("x") as log:
        process = subprocess.Popen(
            [str(binary), str(csv_path), str(markers)], cwd=ROOT, env=env,
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        )
        try:
            result = process.wait(timeout=60 + markers * 0.25)
        finally:
            stop_process(process)
    if result:
        raise RuntimeError(f"client failed ({result}); inspect {log_path}; no automatic retry or exclusion")
    return process.pid


def stats(values):
    values = sorted(values)
    return {"n": len(values), **{name: values[math.ceil(q * len(values)) - 1] for name, q in QUANTILES}}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


class Results:
    def __init__(self, args, mode, **details):
        self.args = args
        self.binaries = dict(args.variant)
        if len(self.binaries) != len(args.variant):
            raise ValueError("variant names must be unique")
        names = list(self.binaries)
        self.order = [name for cycle in range(args.runs) for name in (names if cycle % 2 == 0 else names[::-1])]
        self.out = args.out.resolve()
        self.out.mkdir(parents=True, exist_ok=False)
        binaries = {}
        for name, path in self.binaries.items():
            with path.open("rb") as binary:
                digest = hashlib.sha256()
                for chunk in iter(lambda: binary.read(1024 * 1024), b""):
                    digest.update(chunk)
                digest = digest.hexdigest()
            binaries[name] = {"path": str(path), "sha256": digest}
        self.manifest = {
            "mode": mode, "order": self.order, "markers_per_run": args.markers,
            "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "platform": platform.platform(), "binaries": binaries, "runs": [], **details,
        }
        self.rows = []
        self.save()

    def save(self):
        write_json(self.out / "manifest.json", self.manifest)

    def path(self, run, name):
        return self.out / f"run-{run:02d}-{name}.csv"

    def accept(self, run, name, pid, **details):
        path = self.path(run, name)
        with path.open(newline="") as file:
            rows = list(csv.DictReader(file))
        meta = json.loads(Path(f"{path}.json").read_text())
        matched = len(rows)
        lost = meta["lost_markers"]
        expected = self.args.markers
        # Check both artifacts and marker identities, not just a successful exit.
        markers = [int(row["marker"]) for row in rows]
        if (matched * 20 < expected * 19 or matched != meta["n"] or matched + len(lost) != expected
                or set(markers) & set(lost) or sorted(markers + lost) != list(range(expected))
                or meta.get("markers", expected) != expected):
            raise RuntimeError(f"run {run}: inconsistent output or fewer than 95% of markers matched; see {path}")
        if any(not math.isfinite(float(row["latency_ms"])) or not 0 <= float(row["latency_ms"]) < 250 for row in rows):
            raise RuntimeError(f"run {run}: invalid latency values; see {path}")
        self.rows.extend({"run": run, "variant": name, **row} for row in rows)
        self.manifest["runs"].append({**meta, "run": run, "variant": name, "client_pid": pid, **details})
        self.save()
        print(json.dumps({key: meta[key] for key, _ in QUANTILES}), flush=True)

    def finish(self):
        with (self.out / "latency.csv").open("x", newline="") as file:
            writer = csv.DictWriter(file, fieldnames=list(self.rows[0]), lineterminator="\n")
            writer.writeheader()
            writer.writerows(self.rows)
        variants = {
            name: stats([float(row["latency_ms"]) for row in self.rows if row["variant"] == name])
            for name in self.binaries
        }
        baseline = next(iter(variants))
        summary = {
            "baseline": baseline, "variants": variants,
            "delta_vs_baseline_ms": {
                name: {key: values[key] - variants[baseline][key] for key, _ in QUANTILES}
                for name, values in variants.items() if name != baseline
            },
        }
        write_json(self.out / "summary.json", summary)
        print(json.dumps(summary, indent=2), flush=True)

    def fail(self, error):
        self.manifest["error"] = str(error) or type(error).__name__
        self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    add_arguments(parser)
    args = parser.parse_args()
    install_signals()
    results = Results(args, "local")
    try:
        for run, name in enumerate(results.order, 1):
            print(f"run {run}/{len(results.order)}: {name}", flush=True)
            pid = run_client(results.binaries[name], results.path(run, name), args.markers, environment())
            results.accept(run, name, pid)
        results.finish()
    except BaseException as error:
        results.fail(error)
        raise


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise SystemExit(str(error) or "interrupted") from error
