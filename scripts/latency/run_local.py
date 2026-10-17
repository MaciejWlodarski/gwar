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


def nonnegative(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return number


def minimum_latency(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
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
    parser.add_argument("--minimum-latency-ms", type=minimum_latency, default=5.0, help="reject measurements below this floor (default: 5)")
    parser.add_argument("--extra-delay-ms", type=nonnegative, default=0, help="delay audio in the fake output device (default: 0)")
    parser.add_argument("--out", type=Path, required=True, help="new output directory; never overwritten")


def cancelled(signum, frame):
    raise KeyboardInterrupt(f"signal {signum}")


def install_signals():
    signal.signal(signal.SIGTERM, cancelled)


def environment(args):
    env = {key: value for key, value in os.environ.items() if not key.startswith(("VC_", "LATENCY_"))}
    env.update(LATENCY_MIN_MS=str(args.minimum_latency_ms), LATENCY_EXTRA_DELAY_MS=str(args.extra_delay_ms))
    return env


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
            # Connection wait (<= 30 s), the longest accepted LATENCY_WARMUP_MS
            # (30 s, set by variant wrappers), markers and the match window.
            result = process.wait(timeout=90 + markers * 0.25)
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
            "platform": platform.platform(), "binaries": binaries, "runs": [],
            "minimum_latency_ms": args.minimum_latency_ms, "extra_delay_ms": args.extra_delay_ms, **details,
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
        # Require identity-aware output: older binaries can alias long latency.
        identities = meta["marker_identity_count"]
        window_ms = meta["matching_window_ms"]
        floor_ms = meta["minimum_latency_ms"]
        if (not isinstance(identities, int) or identities < 2
                or len(meta["marker_frequencies_hz"]) != identities
                or not math.isfinite(window_ms) or not math.isfinite(floor_ms)
                or not 0 < floor_ms < window_ms
                or not math.isclose(window_ms, identities * meta["marker_interval_samples"] * 1000 / meta["sample_rate"])
                or floor_ms != self.args.minimum_latency_ms or meta["extra_delay_ms"] != self.args.extra_delay_ms
                or meta["rejected_onsets"] or len(meta["marker_captures"]) != expected):
            raise RuntimeError(f"run {run}: invalid identity window, floor or rejected onsets; see {path}")
        samples = [int(row["playout_sample"]) for row in rows]
        detected = {onset["sample"]: onset for onset in meta["detected_onsets"]}
        if len(set(samples)) != matched:
            raise RuntimeError(f"run {run}: a detected onset was reused; see {path}")
        for row in rows:
            marker = int(row["marker"])
            identity = int(row["identity"])
            latency_ms = float(row["latency_ms"])
            capture_s = float(row["capture_s"])
            playout_s = float(row["playout_s"])
            capture = meta["marker_captures"][marker]
            onset = detected.get(int(row["playout_sample"]))
            if (not all(math.isfinite(value) for value in (latency_ms, capture_s, playout_s))
                    or not floor_ms <= latency_ms < window_ms
                    or identity != marker % identities or identity != capture["identity"]
                    or onset is None or onset["identity"] != identity
                    or not math.isclose(capture_s, capture["capture_s"], rel_tol=0, abs_tol=1e-9)
                    or not math.isclose(playout_s, onset["playout_s"], rel_tol=0, abs_tol=1e-9)
                    or not math.isclose(latency_ms, (playout_s - capture_s) * 1000, rel_tol=0, abs_tol=2e-6)):
                raise RuntimeError(f"run {run}: invalid latency or marker identity; see {path}")
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
            pid = run_client(results.binaries[name], results.path(run, name), args.markers, environment(args))
            results.accept(run, name, pid)
        results.finish()
    except BaseException as error:
        results.fail(error)
        raise


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise SystemExit(str(error) or "interrupted") from error
