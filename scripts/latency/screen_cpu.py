#!/usr/bin/env python3
"""Screen SFU CPU per forwarded Mbps (docs/screen-share.md, "Cost on the deployment VM").

For each viewer count, starts a fresh gwar-server process on loopback and runs the latency
example against it with a LATENCY_SCREEN load (one share), and samples the server process's
CPU time while the viewers' slots are past their ramp. The load and the voice clients run
in the example's process, so the server's figure is its own. The marginal cost is the
slope of CPU (percent of one core) over forwarded Mbps, by least squares.

Run it on the host to be measured (the deployment VM) with both binaries built there:
  cargo build --release -p gwar-server --bin gwar-server
  cargo build --release -p gwar-client --example latency
  python3 scripts/latency/screen_cpu.py --server target/release/gwar-server \\
    --load target/release/examples/latency --out /tmp/screen-cpu
"""

import argparse
import json
import os
import platform
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.dont_write_bytecode = True
from run_local import positive, stop_process, write_json  # noqa: E402

# The load's steady rate starts 12 s after it does (screen.rs STEADY_AFTER); the CPU window
# starts once the example has connected, started the load and passed that.
SETTLE_S = 18.0
MARKER_S = 0.25


def free_port(kind):
    with socket.socket(socket.AF_INET, kind) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def cpu_seconds(pid):
    """User plus system CPU time of a process."""
    stat = Path(f"/proc/{pid}/stat")
    if stat.exists():
        fields = stat.read_text().rsplit(")", 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")
    # macOS and BSD: [[dd-]hh:]mm:ss.ss
    value = subprocess.run(["ps", "-o", "time=", "-p", str(pid)], capture_output=True, text=True, check=True)
    days, _, clock = value.stdout.strip().rpartition("-")
    seconds = 0.0
    for part in clock.split(":"):
        seconds = seconds * 60 + float(part)
    return seconds + int(days or 0) * 86400


def wait_listening(port, process, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError("gwar-server exited during startup")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError("gwar-server did not listen within 30 s")


def step(args, http, media, viewers, out):
    markers = max(1, round((SETTLE_S + args.seconds) / MARKER_S))
    env = {key: value for key, value in os.environ.items() if not key.startswith(("GWAR_", "LATENCY_"))}
    env.update(
        LATENCY_URL=f"ws://127.0.0.1:{http}/ws", LATENCY_SERVER=f"127.0.0.1:{media}",
        LATENCY_WAIT_CONNECTED="1", LATENCY_WARMUP_MS="0",
        LATENCY_SCREEN=f"shares=1,bitrate={args.bitrate},fps={args.fps},keyframe={args.keyframe},viewers={viewers}",
    )
    csv = out / f"viewers-{viewers:02d}.csv"
    samples = []
    with csv.with_suffix(".txt").open("x") as log:
        load = subprocess.Popen([str(args.load), str(csv), str(markers)], env=env, stdin=subprocess.DEVNULL,
                                stdout=log, stderr=subprocess.STDOUT)
        started = time.monotonic()
        try:
            while load.poll() is None:
                samples.append((time.time(), cpu_seconds(args.server_pid)))
                time.sleep(0.5)
                if time.monotonic() - started > SETTLE_S + args.seconds + 120:
                    raise RuntimeError(f"load with {viewers} viewers did not finish; see {csv.with_suffix('.txt')}")
        finally:
            stop_process(load)
    if load.returncode:
        raise RuntimeError(f"load with {viewers} viewers failed; see {csv.with_suffix('.txt')}")
    meta = json.loads(Path(f"{csv}.json").read_text())
    screen = meta["screen"]
    return measurement(samples, screen, viewers, meta["p99_ms"], args.seconds)


def interpolate(samples, at):
    """Cumulative CPU at a load snapshot, bounded by the two surrounding CPU samples."""
    for (t0, c0), (t1, c1) in zip(samples, samples[1:]):
        if t0 <= at <= t1:
            return c0 + (c1 - c0) * (at - t0) / (t1 - t0)
    raise RuntimeError("load timestamp outside CPU sample coverage")


def measurement(cpu, screen, viewers, voice_p99, seconds=None):
    # Both deltas use the exact same settled load snapshot endpoints. Interpolate cumulative
    # CPU between the surrounding 500 ms samples, rather than comparing unrelated windows.
    window = [s for s in screen["samples"]
              if max(screen["settled_unix_s"], cpu[0][0]) <= s["unix_s"] <= cpu[-1][0]]
    if window and seconds is not None:
        window = [s for s in window if s["unix_s"] <= window[0]["unix_s"] + seconds]
    if len(window) < 4:
        raise RuntimeError(f"too few settled load/CPU samples with {viewers} viewers")
    first, last = window[0], window[-1]
    t0, t1 = first["unix_s"], last["unix_s"]
    c0, c1 = interpolate(cpu, t0), interpolate(cpu, t1)
    mbps = (last["received_bytes"] - first["received_bytes"]) * 8 / (t1 - t0) / 1e6
    if mbps <= 0:
        raise RuntimeError("no forwarded screen load in CPU interval")
    return {
        "viewers": viewers, "forwarded_mbps": mbps, "cpu_percent": (c1 - c0) / (t1 - t0) * 100,
        "window_start_unix_s": t0, "window_end_unix_s": t1, "window_s": t1 - t0,
        "pauses": screen["pauses"], "voice_p99_ms": voice_p99,
    }



def measure(args, viewers):
    """One step on a fresh server: the server's per-address limits (16 connections, a burst of
    20 hellos) then hold for each step."""
    http, media = free_port(socket.SOCK_STREAM), free_port(socket.SOCK_DGRAM)
    with tempfile.TemporaryDirectory(prefix="gwar-screen-cpu-") as data, \
            (args.out / f"server-{viewers:02d}.txt").open("x") as log:
        server = subprocess.Popen(
            [str(args.server), "--data-dir", data, "--http", f"127.0.0.1:{http}", "--media", f"127.0.0.1:{media}",
             "--public-ip", "127.0.0.1", "--no-connect", "--insecure-any-host", "--upload-limit-mb", "0"],
            env={key: value for key, value in os.environ.items() if not key.startswith("GWAR_")},
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        )
        args.server_pid = server.pid
        try:
            wait_listening(http, server)
            return step(args, http, media, viewers, args.out)
        finally:
            stop_process(server)


def slope(points):
    xs, ys = [p["forwarded_mbps"] for p in points], [p["cpu_percent"] for p in points]
    mx, my = sum(xs) / len(xs), sum(ys) / len(ys)
    sxx = sum((x - mx) ** 2 for x in xs)
    if sxx == 0:
        return None, None
    k = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx
    return k, my - k * mx


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", type=Path, required=True, help="target/release/gwar-server")
    parser.add_argument("--load", type=Path, required=True, help="target/release/examples/latency")
    parser.add_argument("--viewers", default="1,4,8,13", help="viewer counts, at most 13 (default: 1,4,8,13)")
    parser.add_argument("--bitrate", default="4mbit", help="per share (default: 4mbit, Smooth)")
    parser.add_argument("--fps", type=positive, default=30)
    parser.add_argument("--keyframe", type=positive, default=150000)
    parser.add_argument("--seconds", type=positive, default=20, help="CPU window per step (default: 20)")
    parser.add_argument("--out", type=Path, required=True, help="new output directory")
    args = parser.parse_args()
    if not re.fullmatch(r"\d+(,\d+)*", args.viewers):
        raise ValueError("--viewers takes comma-separated counts")
    for binary in (args.server, args.load):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError(f"missing executable: {binary}; build the server and latency example separately")
    args.server, args.load = args.server.resolve(), args.load.resolve()
    args.out.mkdir(parents=True, exist_ok=False)
    points = []
    for viewers in map(int, args.viewers.split(",")):
        print(f"{viewers} viewers", flush=True)
        points.append(measure(args, viewers))
        print(json.dumps(points[-1]), flush=True)
    per_mbps, base = slope(points)
    summary = {
        "platform": platform.platform(), "machine": platform.machine(), "processor": platform.processor(),
        "cpu_count": os.cpu_count(), "bitrate": args.bitrate, "points": points,
        "cpu_percent_per_mbps": per_mbps, "cpu_percent_at_zero": base,
    }
    write_json(args.out / "summary.json", summary)
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise SystemExit(str(error) or "interrupted") from error
