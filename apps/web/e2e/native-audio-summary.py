#!/usr/bin/env python3
"""Summarize retained native-audio counters and RTP arrival timing (delays in ms)."""
import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path


def window(a, b):
    emitted = b["jitterBufferEmittedCount"] - a["jitterBufferEmittedCount"]
    result = {"seconds": round((b["timestamp"] - a["timestamp"]) / 1000, 3)}
    for name, key in [
        ("actual", "jitterBufferDelay"),
        ("target", "jitterBufferTargetDelay"),
        ("minimum", "jitterBufferMinimumDelay"),
    ]:
        result[name] = round((b[key] - a[key]) / emitted * 1000, 3) if emitted else None
    for name in [
        "concealedSamples",
        "insertedSamplesForDeceleration",
        "removedSamplesForAcceleration",
    ]:
        result[name] = b[name] - a[name]
    result["lost"] = b["packetsLost"] - a["packetsLost"]
    return result


def arrivals(packets):
    deltas = [(b["seconds"] - a["seconds"]) * 1000 for a, b in zip(packets, packets[1:])]
    if not deltas:
        return {}
    ordered = sorted(deltas)
    return {
        "packets": len(packets),
        "min": round(min(deltas), 3),
        "max": round(max(deltas), 3),
        "p95": round(ordered[int((len(ordered) - 1) * .95)], 3),
        "under2ms": sum(d < 2 for d in deltas),
        "initialDeltasMs": [round(d, 3) for d in deltas[:25]],
        "under2msIndices": [i for i, d in enumerate(deltas) if d < 2],
    }


def summarize(file, compact):
    if file.is_dir():
        file /= "report.json"
    report = json.loads(file.read_text())
    result = {
        "run": str(file.parent),
        "options": report.get("options"),
        "browser": report.get("browserVersion"),
        "fixture": report.get("fixture"),
        "fixtureSha256": report.get("fixtureSha256"),
        "harnessSha256": report.get("harnessSha256"),
        "passed": report.get("passed", False),
        "failure": report.get("failure"),
        "phases": [],
    }
    for phase in report["phases"]:
        if "after" not in phase:
            continue
        points = [phase["before"]["inbound"]] + [
            p["browser"]["inbound"] for p in phase["timeline"]
        ]
        windows = [window(a, b) for a, b in zip(points, points[1:])]
        measured = {
            "slot": phase["slot"],
            "mean": window(phase["before"]["inbound"], phase["after"]["inbound"]),
            "lastTenSeconds": window(points[max(0, len(points) - 11)], points[-1]),
            "blocks": [
                {
                    "fromSecond": start,
                    "toSecond": min(end, len(points) - 1),
                    **window(points[start], points[min(end, len(points) - 1)]),
                }
                for start, end in [(0, 10), (10, 30), (30, 50), (50, 60)]
                if start < len(points) - 1
            ],
        }
        if not compact:
            measured["oneSecond"] = windows
        result["phases"].append(measured)
    packets = report.get("rtpTrace") or []
    if (
        packets
        and (report.get("options") or {}).get("scenario") != "continuous"
        and len(report["phases"]) == 2
        and "after" in report["phases"][0]
    ):
        ssrc = report["phases"][0]["after"]["inbound"]["ssrc"]
        down = [p for p in packets if p["direction"] == "down" and p["ssrc"] == ssrc]
        pairs = list(zip(down, down[1:]))
        # Largest RTP leap identifies the original slot's pause/reassignment.
        a, b = max(pairs, key=lambda pair: (pair[1]["timestamp"] - pair[0]["timestamp"]) % 2**32)
        arrival = (b["seconds"] - a["seconds"]) * 1000
        media = (b["timestamp"] - a["timestamp"]) % 2**32 / 48
        after = [p for p in down if b["seconds"] <= p["seconds"] < b["seconds"] + 2]
        result["switch"] = {
            "arrivalMs": round(arrival, 3),
            "mediaMs": media,
            "arrivalMinusMediaMs": round(arrival - media, 3),
            "marker": b.get("marker"),
            "first2s": arrivals(after),
            "timestampDeltas": dict(Counter(
                (q["timestamp"] - p["timestamp"]) % 2**32
                for p, q in zip(after, after[1:])
            )),
        }
        flows = defaultdict(list)
        for packet in packets:
            if packet["direction"] == "up":
                flows[(packet["client"], packet["ssrc"])].append(packet)
        startup = [
            {"flow": str(flow), **arrivals([
                p for p in items if p["seconds"] < items[0]["seconds"] + 2
            ])}
            for flow, items in flows.items()
        ]
        result["uplinkStartup"] = startup[-1:] if compact else startup
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compact", action="store_true", help="Omit per-second windows and intermediate senders")
    parser.add_argument("reports", nargs="+", type=Path)
    args = parser.parse_args()
    for file in args.reports:
        print(json.dumps(summarize(file, args.compact)))


if __name__ == "__main__":
    main()
