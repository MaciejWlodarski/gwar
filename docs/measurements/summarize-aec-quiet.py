"""Summarize every retained quiet timing attempt; never drop a valid failure."""
import argparse
import csv
import json
import statistics
import re
from pathlib import Path

PAIR = re.compile(r'pair (\d+) order (\[[^]]+\]): none p50 ([^ ]+) p99 ([^ ]+) max ([^ ]+) missing (\d+) \| four p50 ([^ ]+) p99 ([^ ]+) max ([^ ]+) missing (\d+) \| p99 delta ([^ ]+) ms')


def write(path, rows):
    if not rows:
        return
    with path.open('w', newline='') as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator='\n')
        writer.writeheader()
        writer.writerows(rows)


def summarize(root, allow_partial=False):
    accepted_path = root / 'accepted.json'
    accepted = json.loads(accepted_path.read_text()) if accepted_path.exists() else []
    expected = ['head', 'main'] * 3
    assert [run['arm'] for run in accepted] == expected[:len(accepted)]
    assert allow_partial or len(accepted) == 6
    pairs, runs, counters = [], [], {'head': 0, 'main': 0}
    attempts = []
    for path in sorted(root.glob('*.meta.json')):
        run = json.loads(path.read_text())
        attempts.append(dict(attempt=path.stem.removesuffix('.meta'), arm=run['arm'], valid=run['valid'],
                             exit_code=run['exit_code'], peak_load_1m=run['peak_load_1m'],
                             invalid_reasons='; '.join(run['invalid_reasons'])))
    for sequence, run in enumerate(accepted):
        arm = run['arm']
        counters[arm] += 1
        number = counters[arm]
        assert run['start_nice'] == run['test_nice'] == 10
        assert run['before']['load'][0] < run['start_limit'] and run['launch']['load'][0] < run['start_limit']
        assert run['peak_load_1m'] <= run['max_limit']
        assert run['valid'] and not run['invalid_reasons']
        candidates = [(path, json.loads(path.read_text())) for path in root.glob('*.meta.json')]
        [meta_path] = [path for path, candidate in candidates if candidate['pid'] == run['pid'] and candidate['before']['at'] == run['before']['at']]
        log = meta_path.with_name(meta_path.name.removesuffix('.meta.json') + '.txt').read_text()
        selected = []
        for line in log.splitlines():
            match = PAIR.search(line)
            if not match:
                continue
            pair, order, b50, b99, bmax, bmissing, l50, l99, lmax, lmissing, delta = match.groups()
            row = dict(sequence=sequence, arm=arm, run=number, sha=run['sha'], pair=int(pair), order=order,
                       none_p50_ms=float(b50), none_p99_ms=float(b99), none_max_ms=float(bmax), none_missing=int(bmissing),
                       four_p50_ms=float(l50), four_p99_ms=float(l99), four_max_ms=float(lmax), four_missing=int(lmissing),
                       delta_ms=float(delta))
            pairs.append(row)
            selected.append(row)
        # A valid early unit failure remains a failure, never a replaceable noisy sample.
        deltas = [row['delta_ms'] for row in selected]
        runs.append(dict(sequence=sequence, arm=arm, run=number, sha=run['sha'],
                         exit_code=run['exit_code'], pairs=len(selected),
                         max_delta_ms=max(deltas) if deltas else '', median_delta_ms=statistics.median(deltas) if deltas else '',
                         mean_delta_ms=statistics.mean(deltas) if deltas else '',
                         start_load_1m=run['before']['load'][0], peak_load_1m=run['peak_load_1m'], end_load_1m=run['after']['load'][0],
                         uptime_before=run['before']['uptime'], uptime_after=run['after']['uptime']))
    comparisons = []
    for i in range(len(runs)//2):
        head, main = runs[2*i:2*i+2]
        comparisons.append(dict(block=i+1, head_max_ms=head['max_delta_ms'], main_max_ms=main['max_delta_ms'],
                                head_median_ms=head['median_delta_ms'], main_median_ms=main['median_delta_ms'],
                                median_difference_ms=head['median_delta_ms']-main['median_delta_ms'] if head['pairs'] and main['pairs'] else '',
                                max_difference_ms=head['max_delta_ms']-main['max_delta_ms'] if head['pairs'] and main['pairs'] else ''))
    write(root / 'pairs.csv', pairs)
    write(root / 'runs.csv', runs)
    write(root / 'comparisons.csv', comparisons)
    write(root / 'attempts.csv', attempts)
    report = [f'Completed valid alternating runs: {len(accepted)}/6. All attempts, including invalid inputs and valid failures, are retained.', '']
    report += ['| Sequence | Revision | Run | Start / max / end load | Max delta, ms | Median delta, ms | Test |',
               '| --- | --- | --- | --- | --- | --- | --- |']
    for run in runs:
        passed = run['exit_code'] == 0 and run['pairs'] == 6
        report.append(f"| {run['sequence']+1} | {run['arm']} | {run['run']} | {run['start_load_1m']:.4f} / {run['peak_load_1m']:.4f} / {run['end_load_1m']:.4f} | {run['max_delta_ms']:+.3f} | {run['median_delta_ms']:+.3f} | {'PASS' if passed else 'FAIL'} |")
    report += ['', '| Run | Pair/order | Head none / four p99, ms | Head delta, ms | Main none / four p99, ms | Main delta, ms |',
               '| --- | --- | --- | --- | --- | --- |']
    lookup = {(row['arm'], row['run'], row['pair']): row for row in pairs}
    for number in range(1, 4):
        for pair in range(6):
            h, m = lookup.get(('head', number, pair)), lookup.get(('main', number, pair))
            if h and m:
                report.append(f"| {number} | {pair}: {h['order']} | {h['none_p99_ms']:.3f} / {h['four_p99_ms']:.3f} | {h['delta_ms']:+.3f} | {m['none_p99_ms']:.3f} / {m['four_p99_ms']:.3f} | {m['delta_ms']:+.3f} |")
    report += ['', '| Adjacent head/main block | Median delta difference, ms | Max delta difference, ms |', '| --- | --- | --- |']
    for row in comparisons:
        report.append(f"| {row['block']} | {row['median_difference_ms']:+.3f} | {row['max_difference_ms']:+.3f} |")
    (root / 'report.md').write_text('\n'.join(report) + '\n')
    for arm in ['head', 'main']:
        rows = [row for row in pairs if row['arm'] == arm]
        if not rows:
            print(arm, 'no pair data yet')
            continue
        print(arm, 'pairs', len(rows), 'max', max(row['delta_ms'] for row in rows),
              'median', statistics.median(row['delta_ms'] for row in rows),
              'none p99 range', min(row['none_p99_ms'] for row in rows), max(row['none_p99_ms'] for row in rows))
    print(json.dumps(comparisons, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('root', type=Path)
    parser.add_argument('--allow-partial', action='store_true')
    args = parser.parse_args()
    summarize(args.root, args.allow_partial)
