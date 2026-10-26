#!/usr/bin/env python3
"""Screen-sharing acceptance for voice (docs/screen-share.md, "Voice headroom and acceptance").

Each scenario interleaves A/A pairs (both arms without screen) and screen pairs (baseline
without screen, second arm with LATENCY_SCREEN). A four-run block shares its LATENCY_NET
seed. Both set order and arm order reverse in alternate blocks. Each screen-set metric
must be <= its A/A counterpart plus the tolerance:

- unconstrained: median per-pair p95/p99 deltas + 2 ms; total underrun excess + 0;
- constrained: median per-pair p99 delta + 20 ms; median underrun-fraction delta + 0.5 pp;
- both: tail-pair count + floor(pairs / 10), using the original absolute per-pair rules.

This is an empirical comparison, not a statistical confidence claim. Eight pairs per set
and 100 markers plan to about 2.6 hours for all six scenarios on a 4-vCPU host, excluding
the build. --null also removes screen from the screen set: two interleaved A/A sets for
diagnosing repeatability. It does not establish acceptance under load.
Exits 1 when a scenario misses the gate or a run/load fails; summary.json has both sets.
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
        **device_health(meta),
    }


def device_health(meta):
    """Harness health, not a gate input: a stalled fake device can force underruns of its own.
    Runs from binaries before these diagnostics have none; they are reported unavailable."""
    device, record = meta.get("receiver_device"), meta.get("receiver_record")
    return {
        "device_timing": "available" if device else "unavailable",
        "device_late_over_5ms": device["late_over_5ms"] if device else None,
        "device_max_busy_ms": device["max_busy_ms"] if device else None,
        "record_overflow_samples": record["overflow_samples"] if record else None,
    }


def metrics(pairs, constrained):
    """Same descriptive metrics for each set; arm deltas remain signed, without clipping."""
    delta = {key: statistics.median(p["screen"][key] - p["baseline"][key] for p in pairs)
             for key in ("median_ms", "p95_ms", "p99_ms", "underrun_fraction")}
    underruns = {name: sum(p[name]["underruns"] for p in pairs) for name in ("baseline", "screen")}

    def regresses(pair):
        a, b = pair["baseline"], pair["screen"]
        if constrained:
            return (b["p99_ms"] - a["p99_ms"] > CONSTRAINED_MS
                    or b["underrun_fraction"] - a["underrun_fraction"] > UNDERRUN_FRACTION)
        return (b["p95_ms"] - a["p95_ms"] > UNCONSTRAINED_MS
                or b["p99_ms"] - a["p99_ms"] > UNCONSTRAINED_MS
                or b["underruns"] > a["underruns"])
    tail_pairs = sum(regresses(p) for p in pairs)
    return {"median_delta": delta, "underruns": underruns, "tail_pairs": tail_pairs,
            "underrun_excess": underruns["screen"] - underruns["baseline"]}


def gate(aa_pairs, screen_pairs, constrained):
    """Compare equal-sized interleaved sets; never accept missing calibration."""
    if not aa_pairs or len(aa_pairs) != len(screen_pairs):
        raise ValueError("calibration needs equal, nonempty A/A and screen sets")
    sets = {name: metrics(pairs, constrained) for name, pairs in
            (("aa", aa_pairs), ("screen", screen_pairs))}
    aa, screen = sets["aa"], sets["screen"]
    tolerances = {"p99_ms": CONSTRAINED_MS if constrained else UNCONSTRAINED_MS}
    if not constrained:
        tolerances["p95_ms"] = UNCONSTRAINED_MS
    tolerances["underrun_fraction" if constrained else "underrun_excess"] = UNDERRUN_FRACTION if constrained else 0
    tolerances["tail_pairs"] = len(aa_pairs) // 10
    comparison = {}
    for metric, tolerance in tolerances.items():
        a = aa[metric] if metric in aa else aa["median_delta"][metric]
        s = screen[metric] if metric in screen else screen["median_delta"][metric]
        limit = a + tolerance
        comparison[metric] = {"aa": a, "screen": s, "tolerance": tolerance,
                              "limit": limit, "screen_minus_aa": s - a, "pass": s <= limit}
    checks = {metric: values["pass"] for metric, values in comparison.items()}
    return {"sets": sets, "comparison": comparison, "checks": checks, "pass": all(checks.values())}


def block_order(pair):
    """Keep matched A/A and screen pairs adjacent and balance both orders over two blocks."""
    sets = ("aa", "screen") if pair % 2 == 0 else ("screen", "aa")
    arms = ("baseline", "screen") if pair % 2 == 0 else ("screen", "baseline")
    return [(set_name, arm) for set_name in sets for arm in arms]


def runtime_estimate(scenarios, pairs, markers):
    """Planning estimate from latency.rs run_for; five seconds per run for setup/teardown."""
    runs = scenarios * pairs * 4
    measurement_s = 1 + WARMUP_MS / 1000 + markers * 0.25 + 2
    return {"runs": runs, "measurement_seconds_per_run": measurement_s,
            "setup_allowance_seconds_per_run": 5, "estimated_hours": runs * (measurement_s + 5) / 3600}


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
    if not constrained and load.get("pauses") != 0:
        raise RuntimeError("unconstrained screen load must report zero pauses")
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
        return {"status": "not_applicable", "reason": "direct media path; LATENCY_NET relay is disabled"}
    return {"status": "measured", **{
        direction: {key: network[direction][key] for key in
                    ("queue_max_ms", "scheduling_lateness_max_ms", "scheduling_late_over_5ms")}
        for direction in ("up", "down")}}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=lambda v: variant(f"latency={v}")[1], required=True,
                        help="target/release/examples/latency")
    parser.add_argument("--scenario", choices=SCENARIOS, action="append", help="repeatable (default: all)")
    parser.add_argument("--null", action="store_true", help="diagnostic: both interleaved sets are A/A; no acceptance under load")
    parser.add_argument("--pairs", type=positive, default=8, help="pairs PER SET per scenario (default: 8; four runs per pair index)")
    parser.add_argument("--markers", type=positive, default=100, help="markers per run (default: 100)")
    parser.add_argument("--minimum-latency-ms", type=minimum_latency, default=5.0)
    parser.add_argument("--out", type=Path, required=True, help="new output directory; never overwritten")
    args = parser.parse_args()
    install_signals()
    args.out.mkdir(parents=True, exist_ok=False)
    summary = {"rule": "interleaved-aa-v1", "null": args.null, "pairs_per_set": args.pairs,
               "markers_per_run": args.markers, "warmup_ms": WARMUP_MS,
               "runtime_estimate": runtime_estimate(len(args.scenario or SCENARIOS), args.pairs, args.markers),
               "scenarios": {}, "pass": False}
    write_json(args.out / "summary.json", summary)
    print(json.dumps({"runtime_estimate": summary["runtime_estimate"]}), flush=True)
    for name in args.scenario or list(SCENARIOS):
        net, screen, constrained = SCENARIOS[name]
        sets = {set_name: {"pairs": []} for set_name in ("aa", "screen")}
        scenario = {"net": net, "screen": None if args.null else screen, "constrained": constrained,
                    "sets": sets, "execution_order": [], "pass": False}
        summary["scenarios"][name] = scenario
        results = {}
        try:
            for set_name in sets:
                options = SimpleNamespace(
                    variant=[("baseline", args.binary), ("screen", args.binary)], runs=args.pairs,
                    markers=args.markers, minimum_latency_ms=args.minimum_latency_ms, extra_delay_ms=0,
                    out=args.out / name / set_name,
                )
                results[set_name] = Results(options, "screen-gate", scenario=name, set_name=set_name,
                                            net=net, screen=screen if set_name == "screen" and not args.null else None,
                                            constrained=constrained, null=args.null)
            for pair in range(args.pairs):
                figures = {set_name: {} for set_name in sets}
                for set_name, which in block_order(pair):
                    result = results[set_name]
                    run = len(result.manifest["runs"]) + 1
                    loaded = set_name == "screen" and which == "screen" and not args.null
                    print(f"{name} block {pair + 1}/{args.pairs}: {set_name}/{which}", flush=True)
                    env = environment(result.args)
                    env.update(LATENCY_WAIT_CONNECTED="1", LATENCY_WARMUP_MS=str(WARMUP_MS))
                    env.setdefault("RUST_LOG", "gwar_server::media::screen=debug")
                    if net:
                        env["LATENCY_NET"] = f"seed={pair + 1};{net}"
                    if loaded:
                        env["LATENCY_SCREEN"] = screen
                    path = result.path(run, which)
                    pid = run_client(args.binary, path, args.markers, env)
                    result.accept(run, which, pid, pair=pair, seed=pair + 1)
                    meta = json.loads(Path(f"{path}.json").read_text())
                    if not loaded and meta.get("screen") is not None:
                        raise RuntimeError("unexpected screen load in no-load arm")
                    load_check = validate_load(meta, screen, constrained) if loaded else None
                    figures[set_name][which] = {**playout(meta), "screen": meta.get("screen"),
                                               "load_check": load_check, "relay_timing": relay_timing(meta)}
                    scenario["execution_order"].append({"pair": pair, "set": set_name, "arm": which, "seed": pair + 1})
                for set_name in sets:
                    sets[set_name]["pairs"].append(figures[set_name])
                write_json(args.out / "summary.json", summary)
            for result in results.values():
                result.finish()
        except BaseException as error:
            for result in results.values():
                result.fail(error)
            scenario["error"] = str(error) or type(error).__name__
            write_json(args.out / "summary.json", summary)
            raise
        verdict = gate(sets["aa"]["pairs"], sets["screen"]["pairs"], constrained)
        for set_name, values in verdict.pop("sets").items():
            sets[set_name].update(values)
        scenario.update(verdict)
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
