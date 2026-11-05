#!/usr/bin/env python3
"""Summarize the post-merge voice audit's F1/F5 candidate measurements without building anything.

Run from the repository root: python3 docs/measurements/summarize-aec-audit.py
"""
import csv
import math
import re
from collections import defaultdict

D = "docs/measurements/"

approved, energy = {}, {}
for row in csv.reader(open(D + "aec-paired-echo-cases.csv")):
    if row[0] == "7":
        approved[row[1]] = (float(row[2]), float(row[3]))
for row in csv.DictReader(open(D + "aec-shipped-echo.csv")):
    energy[row["case_window"]] = float(row["energy-candidate"])

PAIR = re.compile(r"native-echo-pair,(.*),current=([-\d.]+),candidate=([-\d.]+),.*energy-candidate=([-\d.e+]+),energy-cold")
for name in ("mid", "recent"):
    rows = [PAIR.match(line).groups() for line in open(D + f"aec-audit-{name}-echo.txt")]
    rows = [(case, float(main), float(level), float(e)) for case, main, level, e in rows]
    assert len(rows) == 549 and all(main == approved[case][0] for case, main, _, _ in rows), "main fixture changed"
    deltas = [(level - approved[case][1], case) for case, _, level, _ in rows]
    over = [sum(d > limit for d, _ in deltas) for limit in (1, 3, 6)]
    total = 10 * math.log10(sum(e for *_, e in rows) / sum(energy[case] for case, *_ in rows))
    worst = max(deltas)
    print(f"{name} echo vs approved P7: >+1/+3/+6 dB {over[0]}/{over[1]}/{over[2]}, worst {worst[0]:+.3f} dB ({worst[1]}), energy {total:+.3f} dB")

right = defaultdict(lambda: defaultdict(float))
for line in open(D + "aec-audit-mid-right.txt"):
    _, arm, case, _, e = line.rstrip("\n").split("\t")
    right[case.split(",")[0]][arm] += float(e)
for shape, arms in right.items():
    print(f"mid {shape} energy vs shipped: {10 * math.log10(arms['mid'] / arms['shipped']):+.3f} dB")

for line in open(D + "aec-audit-recent-routes.txt"):
    if line.startswith("recent-route-mean"):
        print(line.rstrip("\n").replace("\t", " | "))
