#!/usr/bin/env python3
"""Screen-sharing acceptance for voice (docs/screen-share.md, "Voice headroom and acceptance").

Paired runs of the latency example, without and with a LATENCY_SCREEN load, alternating
which goes first, per scenario. Each pair shares its LATENCY_NET seed. The gate compares
the median of the per-pair deltas:

- unconstrained (no rate limit): p95 and p99 within 2 ms of the baseline, and no more
  playout underruns than the baseline in total;
- constrained (a rate= bottleneck): p99 within 20 ms of the baseline, and the underrun
  fraction (underrun samples / callback samples) within 0.5 percentage points of it.

Exits 1 when a scenario misses the gate; summary.json has the numbers.
"""

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from run_local import (  # noqa: E402
    Results, environment, install_signals, minimum_latency, positive, run_client, variant, write_json,
)

SHARE_8M = "bitrate=8mbit,fps=60,keyframe=200000"
# The spec's 4 Mbps shares cannot fit a 5 Mbps downlink at all: B is 80 % of an estimate that
# stays below the link rate, so both would pause and leave no video beside the voice. 3 Mbps
# Smooth shares keep one share flowing while the other is paused.
SHARE_3M = "bitrate=3mbit,fps=30,keyframe=150000"
LOSS = "loss=1%"
# name: (LATENCY_NET without its seed, or None for the direct path; LATENCY_SCREEN; constrained)
SCENARIOS = {
    "unconstrained": (None, f"shares=1,{SHARE_8M},viewers=5,via=none", False),
    "unconstrained-loss": (f"up:{LOSS};down:{LOSS}", f"shares=1,{SHARE_8M},viewers=5,via=all", False),
    "uplink": ("up:rate=10mbit", f"shares=1,{SHARE_8M},viewers=1,via=publishers", True),
    "uplink-loss": (f"up:rate=10mbit,{LOSS};down:{LOSS}", f"shares=1,{SHARE_8M},viewers=1,via=publishers", True),
    "downlink": ("down:rate=5mbit", f"shares=2,{SHARE_3M},viewers=1,via=viewers", True),
    "downlink-loss": (f"up:{LOSS};down:rate=5mbit,{LOSS}", f"shares=2,{SHARE_3M},viewers=1,via=viewers", True),
}
UNCONSTRAINED_MS = 2.0
CONSTRAINED_MS = 20.0
UNDERRUN_FRACTION = 0.005
# Markers start after the screen load's slots are past their ramp (10 s) and settled.
WARMUP_MS = 15000


def playout(meta):
    """Voice figures of one run, as the latency docs report them."""
    receiver = meta["receiver_playout"]
    slots = receiver["slots"]
    return {
        "median_ms": meta["median_ms"], "p95_ms": meta["p95_ms"], "p99_ms": meta["p99_ms"],
        "lost_markers": len(meta["lost_markers"]),
        "underruns": receiver["underruns"],
        "underrun_fraction": receiver["underrun_samples"] / max(receiver["callback_samples"], 1),
        "concealments": sum(slot["concealments"] for slot in slots),
        "late_drops": sum(slot["late_drops"] for slot in slots),
        "frames_received": meta["receiver_frames_received"],
    }


def gate(pairs, constrained):
    delta = {key: statistics.median(p["screen"][key] - p["baseline"][key] for p in pairs)
             for key in ("median_ms", "p95_ms", "p99_ms", "underrun_fraction")}
    underruns = {name: sum(p[name]["underruns"] for p in pairs) for name in ("baseline", "screen")}
    if constrained:
        checks = {
            f"p99 within {CONSTRAINED_MS} ms": delta["p99_ms"] <= CONSTRAINED_MS,
            "underruns within baseline + 0.5%": delta["underrun_fraction"] <= UNDERRUN_FRACTION,
        }
    else:
        checks = {
            f"p95 within {UNCONSTRAINED_MS} ms": delta["p95_ms"] <= UNCONSTRAINED_MS,
            f"p99 within {UNCONSTRAINED_MS} ms": delta["p99_ms"] <= UNCONSTRAINED_MS,
            "no extra underruns": underruns["screen"] <= underruns["baseline"],
        }
    return {"median_delta": delta, "underruns": underruns, "checks": checks, "pass": all(checks.values())}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=lambda v: variant(f"latency={v}")[1], required=True,
                        help="target/release/examples/latency")
    parser.add_argument("--scenario", choices=SCENARIOS, action="append", help="repeatable (default: all)")
    parser.add_argument("--pairs", type=positive, default=10, help="pairs per scenario (default: 10)")
    parser.add_argument("--markers", type=positive, default=100, help="markers per run (default: 100)")
    parser.add_argument("--minimum-latency-ms", type=minimum_latency, default=5.0)
    parser.add_argument("--out", type=Path, required=True, help="new output directory; never overwritten")
    args = parser.parse_args()
    install_signals()
    args.out.mkdir(parents=True, exist_ok=False)
    summary = {"pairs": args.pairs, "markers_per_run": args.markers, "warmup_ms": WARMUP_MS, "scenarios": {}}
    for name in args.scenario or list(SCENARIOS):
        net, screen, constrained = SCENARIOS[name]
        options = SimpleNamespace(
            variant=[("baseline", args.binary), ("screen", args.binary)], runs=args.pairs * 2,
            markers=args.markers, minimum_latency_ms=args.minimum_latency_ms, extra_delay_ms=0,
            out=args.out / name,
        )
        results = Results(options, "screen-gate", scenario=name, net=net, screen=screen, constrained=constrained)
        pairs = []
        try:
            for pair in range(args.pairs):
                order = ("baseline", "screen") if pair % 2 == 0 else ("screen", "baseline")
                figures = {}
                for which in order:
                    run = len(results.manifest["runs"]) + 1
                    print(f"{name} pair {pair + 1}/{args.pairs}: {which}", flush=True)
                    env = environment(options)
                    env.update(LATENCY_WAIT_CONNECTED="1", LATENCY_WARMUP_MS=str(WARMUP_MS))
                    if net:
                        env["LATENCY_NET"] = f"seed={pair + 1};{net}"
                    if which == "screen":
                        env["LATENCY_SCREEN"] = screen
                    path = results.path(run, which)
                    pid = run_client(args.binary, path, args.markers, env)
                    results.accept(run, which, pid, pair=pair)
                    meta = json.loads(Path(f"{path}.json").read_text())
                    figures[which] = {**playout(meta), "screen": meta.get("screen")}
                pairs.append(figures)
            results.finish()
        except BaseException as error:
            results.fail(error)
            raise
        verdict = gate(pairs, constrained)
        summary["scenarios"][name] = {"net": net, "screen": screen, "constrained": constrained,
                                      "pairs": pairs, **verdict}
        write_json(args.out / "summary.json", summary)
        print(json.dumps({name: verdict}, indent=2), flush=True)
    failed = [name for name, s in summary["scenarios"].items() if not s["pass"]]
    summary["pass"] = not failed
    write_json(args.out / "summary.json", summary)
    if failed:
        raise SystemExit(f"gate missed: {', '.join(failed)}; see {args.out / 'summary.json'}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        raise SystemExit(str(error) or "interrupted") from error
