#!/usr/bin/env python3
"""Voice over a congested, deeply buffered sender uplink (docs/latency-congested-uplink.md).

For each bottleneck (`up10`, `up20` in apps/web/e2e/latency.mjs: 400 ms drop-tail buffer at line
rate, 20 ms base round trip) it runs four interleaved cells through the web latency harness:
voice alone, voice with a screen share, voice with a foreign bulk TCP upload, and all three.
The bulk upload runs from 10 s to 30 s after the first marker. Each bottleneck is one
`latency.mjs` invocation (fresh server, relay and browser per run), so cells alternate in
A B C D D C B A order. It then prints a Markdown table of the medians across runs.

Build first:
  cargo build --release -p gwar-client --example web_latency
Run:
  python3 scripts/latency/congested_uplink.py --runs 3 --out target/congested-uplink
  python3 scripts/latency/congested_uplink.py --table target/congested-uplink   # table only
Extra `--variant`s (latency.mjs syntax, applied to every bottleneck) can compare builds, e.g.
  --only screen --extra 'screen-old:screen=on,web=/tmp/old-dist'
"""
import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CELLS = {
    "voice": "",
    "screen": "screen=on",
    "bulk": "bulk=10-30",
    "screen-bulk": "screen=on,bulk=10-30",
}


def run(args):
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=False)
    cells = [c for c in CELLS if not args.only or c in args.only]
    for link in args.links:
        command = ["pnpm", "--filter", "web", "latency", "--runs", str(args.runs), "--markers", str(args.markers),
                   "--out", str(out / link), "--set", f"net={link},path={args.path}"]
        for cell in cells:
            command += ["--variant", f"{cell}:{CELLS[cell]}" if CELLS[cell] else cell]
        for extra in args.extra:
            command += ["--variant", extra]
        print("+", " ".join(command), flush=True)
        # A failed run is kept in the evidence; the table still reports how many passed.
        subprocess.run(command, cwd=ROOT, check=False)
    table(out)
    curve(out)


def fmt(value, digits=0):
    return "–" if not isinstance(value, (int, float)) else f"{value:.{digits}f}"


def table(out):
    rows = []
    for summary in sorted(Path(out).glob("*/summary.json")):
        link = summary.parent.name
        for row in json.loads(summary.read_text()):
            c = row.get("congestion") or {}
            if not c.get("voice"):
                # No screen and no bulk upload: the whole run, from the harness's own summary.
                st = row.get("stats") or {}
                rows.append([link, row["variant"], "whole run", f"{row['passed']}/{row['runs']}", fmt(row.get("median_ms")), fmt(row.get("p95_ms")),
                             fmt(row.get("p99_ms")), fmt(row["markers"] - row["matched"]), fmt(st.get("jitterBufferMs")), "–",
                             fmt(st.get("concealmentPercent"), 2)] + ["–"] * 7)
                continue
            voice, jb, screen, queue = c.get("voice") or {}, c.get("jitterBuffer") or {}, c.get("screen") or {}, c.get("queue") or {}
            for phase in ("pre", "during", "after"):
                v = voice.get(phase)
                if not v:
                    continue
                s, j, q = screen.get(phase) or {}, jb.get(phase) or {}, queue.get(phase) or {}
                rows.append([
                    link, row["variant"], phase, f"{row['passed']}/{row['runs']}",
                    fmt(v.get("p50_ms")), fmt(v.get("p95_ms")), fmt(v.get("p99_ms")), fmt(v.get("lost")),
                    fmt(j.get("jitterBufferMs")), fmt(j.get("maxMs")), fmt(j.get("concealedPercent"), 2),
                    fmt(q.get("meanMs")), fmt(q.get("maxMs")),
                    fmt(s.get("mbps"), 2), fmt(s.get("fps")), fmt(s.get("height")), fmt(s.get("rttMs")), fmt(s.get("availableMbps"), 2),
                ])
            recovery = [
                ("voice", (voice.get("recovery") or {})),
                ("NetEq", (jb.get("recovery") or {})),
                ("screen rate", (screen.get("rateRecovery") or {})),
                ("screen RTT", (screen.get("rttRecovery") or {})),
            ]
            if any(r for _, r in recovery):
                parts = [f"{name} {fmt((r.get('ms') or 0) / 1000, 1)} s ({r.get('recovered')} recovered)" for name, r in recovery if r]
                rows.append([link, row["variant"], "recovery", "", "; ".join(parts)] + [""] * 13)
    header = ["link", "cell", "phase", "runs", "voice p50", "p95", "p99", "lost", "NetEq ms", "NetEq max", "conceal %",
              "queue mean", "queue max", "screen Mb/s", "fps", "height", "screen RTT", "estimate Mb/s"]
    print("| " + " | ".join(header) + " |")
    print("|" + "---|" * len(header))
    for r in rows:
        print("| " + " | ".join(str(x) for x in r) + " |")


def curve(out):
    """Medians in 5 s bins around the bulk upload's end, pooled over each cell's runs."""
    bins = list(range(-25, 30, 5))
    print("\n| link | cell | series | " + " | ".join(f"{b:+d}..{b + 5:+d} s" for b in bins) + " |")
    print("|" + "---|" * (len(bins) + 3))
    for manifest in sorted(Path(out).glob("*/manifest.json")):
        link = manifest.parent.name
        cells = {}
        for run in sorted(manifest.parent.glob("*/run.json")):
            r = json.loads(run.read_text())
            c = r.get("congestion") or {}
            if not r.get("passed") or not (c.get("bulk") or {}).get("off"):
                continue
            off = c["bulk"]["off"]
            series = cells.setdefault(r["variant"], {"voice ms": [], "NetEq ms": [], "screen Mb/s": [], "estimate Mb/s": [], "screen RTT ms": []})
            for line in (run.parent / "latency.csv").read_text().splitlines()[1:]:
                f = line.split(",")
                series["voice ms"].append(((float(f[2]) - off) / 1000, float(f[4])))
            series["NetEq ms"] += [((x["t"] - off) / 1000, x["jitterBufferMs"]) for x in r.get("jitterBufferSeries") or []]
            for x in (r.get("screenSeries") or {}).get("sent") or []:
                series["screen Mb/s"].append(((x["t"] - off) / 1000, x["mbps"]))
                series["estimate Mb/s"].append(((x["t"] - off) / 1000, x.get("availableMbps")))
                series["screen RTT ms"].append(((x["t"] - off) / 1000, x["rttMs"]))
        for cell, series in cells.items():
            for name, points in series.items():
                if not points:
                    continue
                values = []
                for b in bins:
                    v = sorted(y for t, y in points if b <= t < b + 5 and isinstance(y, (int, float)) and y == y)
                    values.append(fmt(v[len(v) // 2], 2 if "Mb/s" in name else 0) if v else "–")
                print(f"| {link} | {cell} | {name} | " + " | ".join(values) + " |")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--runs", type=int, default=3)
    parser.add_argument("--markers", type=int, default=240, help="60 s: 10 s before, 20 s of bulk upload, 30 s after")
    parser.add_argument("--links", nargs="+", default=["up10", "up20"])
    parser.add_argument("--path", default="browser", choices=["browser", "native-browser", "browser-native"])
    parser.add_argument("--only", nargs="*", choices=list(CELLS))
    parser.add_argument("--extra", action="append", default=[])
    parser.add_argument("--out")
    parser.add_argument("--table", help="print the table of an earlier --out and exit")
    args = parser.parse_args()
    if args.table:
        table(args.table)
        curve(args.table)
    elif not args.out:
        sys.exit("--out is required")
    else:
        run(args)


if __name__ == "__main__":
    main()
