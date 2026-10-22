#!/usr/bin/env python3
"""Screen-sharing acceptance for voice (docs/screen-share.md, "Voice headroom and acceptance").

Paired runs of the latency example, without and with a LATENCY_SCREEN load, alternating
which goes first, per scenario. Each pair shares its LATENCY_NET seed. The gate compares
the median of the per-pair deltas and allows at most one threshold violation per ten
pairs (zero for a shortened run of fewer than ten pairs):

- unconstrained (no rate limit): p95 and p99 within 2 ms of the baseline, and no more
  playout underruns than the baseline in total;
- constrained (a rate= bottleneck): p99 within 20 ms of the baseline, and the underrun
  fraction (underrun samples / samples played) within 0.5 percentage points of it.

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


def played(meta):
    """Samples the receiver played after connecting: the run's length, as latency.rs runs it."""
    rate = meta["sample_rate"]
    return (rate + meta["warmup_samples"] + meta["markers"] * meta["marker_interval_samples"]
            + meta["matching_window_ms"] * rate / 1000)


def playout(meta):
    """Voice figures of one run, as the latency docs report them."""
    receiver = meta["receiver_playout"]
    slots = receiver["slots"]
    return {
        "median_ms": meta["median_ms"], "p95_ms": meta["p95_ms"], "p99_ms": meta["p99_ms"],
        "lost_markers": len(meta["lost_markers"]),
        "underruns": receiver["underruns"],
        "underrun_fraction": receiver["underrun_samples"] / played(meta),
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
    def regresses(pair):
        a, b = pair["baseline"], pair["screen"]
        if constrained:
            return (b["p99_ms"] - a["p99_ms"] > CONSTRAINED_MS
                    or b["underrun_fraction"] - a["underrun_fraction"] > UNDERRUN_FRACTION)
        return (b["p95_ms"] - a["p95_ms"] > UNCONSTRAINED_MS
                or b["p99_ms"] - a["p99_ms"] > UNCONSTRAINED_MS
                or b["underruns"] > a["underruns"])
    tail_pairs = sum(regresses(p) for p in pairs)
    allowed = len(pairs) // 10
    checks[f"at most {allowed} regressing pairs"] = tail_pairs <= allowed
    return {"median_delta": delta, "underruns": underruns, "tail_pairs": tail_pairs,
            "allowed_tail_pairs": allowed, "checks": checks, "pass": all(checks.values())}


def validate_load(meta, expected, constrained):
    """A quiet SFU cannot pass the voice-under-load gate. Every viewer must carry video."""
    load = meta.get("screen")
    if not isinstance(load, dict) or load.get("spec", {}).get("source") != expected:
        raise RuntimeError("missing or unexpected screen load spec")
    spec = load["spec"]
    expected_fields = dict(option.split("=") for option in expected.split(","))
    bitrate = expected_fields["bitrate"]
    expected_fields["bitrate"] = int(float(bitrate.removesuffix("mbit")) * 1e6)
    for field in ("shares", "fps", "keyframe", "viewers"):
        expected_fields[field] = int(expected_fields[field])
    if any(spec.get(key) != value for key, value in expected_fields.items()):
        raise RuntimeError("screen load fields differ from the expected spec")
    # Downlink scenarios intentionally keep one of the two 3 Mbps shares running.
    active_shares = 1 if constrained else spec["shares"]
    minimum = spec["bitrate"] * active_shares / 1e6 * 0.6
    rates = load.get("steady_viewer_mbps", [])
    samples = load.get("samples", [])
    if (len(rates) != spec["viewers"] or not samples
            or samples[-1]["unix_s"] - samples[0]["unix_s"] < 1.0
            or any(not isinstance(rate, (int, float)) or not minimum <= rate < float("inf") for rate in rates)):
        raise RuntimeError(f"insufficient steady screen load: need all {spec['viewers']} viewers >= {minimum:.2f} Mbps; got {rates}; receive scheduling: {load.get('receive_scheduling')}; transitions: {load.get('transitions')}")
    return {"active_viewers": len(rates), "minimum_mbps_per_viewer": minimum, "steady_viewer_mbps": rates}


def relay_timing(meta):
    network = meta.get("network")
    if not network:
        return None
    return {direction: {key: network[direction][key] for key in
            ("queue_max_ms", "scheduling_lateness_max_ms", "scheduling_late_over_5ms")}
            for direction in ("up", "down")}


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
                    env.setdefault("RUST_LOG", "gwar_server::media::screen=debug")
                    if net:
                        env["LATENCY_NET"] = f"seed={pair + 1};{net}"
                    if which == "screen":
                        env["LATENCY_SCREEN"] = screen
                    path = results.path(run, which)
                    pid = run_client(args.binary, path, args.markers, env)
                    results.accept(run, which, pid, pair=pair)
                    meta = json.loads(Path(f"{path}.json").read_text())
                    load_check = validate_load(meta, screen, constrained) if which == "screen" else None
                    figures[which] = {**playout(meta), "screen": meta.get("screen"),
                                      "load_check": load_check, "relay_timing": relay_timing(meta)}
                pairs.append(figures)
            results.finish()
        except BaseException as error:
            results.fail(error)
            raise
        verdict = gate(pairs, constrained)
        verdict["relay_timing"] = {which: [p[which]["relay_timing"] for p in pairs]
                                   for which in ("baseline", "screen")}
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
