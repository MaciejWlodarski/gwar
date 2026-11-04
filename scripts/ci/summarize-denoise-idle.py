"""Extract readiness/quality records from downloaded GitHub job logs; no tests run."""
import json
from pathlib import Path
import re
import statistics
import sys

joins, failures, quality = [], [], []
for name in sys.argv[1:]:
    variant = scenario = ""
    for line in Path(name).read_text().splitlines():
        match = re.search(r"VARIANT (main|pr)\b", line)
        if match:
            variant = match[1]
        if "SCENARIO " in line:
            scenario = line.split("SCENARIO ", 1)[1].strip()
        if not any(marker in line for marker in ['"type":"DENOISE_JOIN"', '"type":"DENOISE_MEASURE"', '"status":"fail"']):
            continue
        record = {"source": Path(name).name, "variant": variant, **json.loads(line[line.index("{"):])}
        if record.get("type") == "DENOISE_JOIN":
            joins.append(record)
        elif record.get("type") == "DENOISE_MEASURE":
            quality.append(record)
        else:
            failures.append({"scenario": scenario, **record})
if not joins:
    raise SystemExit("No DENOISE_JOIN records: check the downloaded logs before interpreting results")
summary = []
for engine in sorted({record["engine"] for record in joins}):
    for scenario in ["cold", "warm"]:
        for variant in ["main", "pr"]:
            records = [r for r in joins if (r["engine"], r["scenario"], r["variant"]) == (engine, scenario, variant)]
            values = [r["joins"][0]["readyMs"] for r in records]
            if values:
                summary.append({"engine": engine, "scenario": scenario, "variant": variant, "count": len(values), "medianMs": statistics.median(values), "minMs": min(values), "maxMs": max(values), "idleLongTasks": sum(len(r["idleLongTasks"]) for r in records)})
print(json.dumps({"summary": summary, "joins": joins, "failures": failures, "quality": quality}, indent=2))
