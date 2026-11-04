"""Regenerate shipped-P7 CSVs from committed raw logs, without audio playback."""
import csv
import math
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def write(name, rows):
    with (ROOT / name).open('w', newline='') as output:
        writer = csv.DictWriter(output, fieldnames=list(rows[0]), lineterminator='\n')
        writer.writeheader()
        writer.writerows(rows)


def speech():
    rows = []
    for line in (ROOT / 'aec-shipped-speech.txt').read_text().splitlines():
        if 'shipped-speech,' not in line:
            continue
        values = dict(part.split('=', 1) for part in line.split('shipped-speech,', 1)[1].split(','))
        row = {key: (value if key == 'echo' else float(value) if 'kept' in key and 'frames' not in key else int(value))
               for key, value in values.items()}
        assert row['hold'] <= row['cold-hold']
        deficit = row['cold-kept-frames'] - row['kept-frames']
        assert deficit <= row['approved-frame-exception']
        row['strict_pass'] = deficit <= 0
        rows.append(row)
    assert len(rows) == 12
    assert sum(not row['strict_pass'] for row in rows) == 1
    write('aec-shipped-speech.csv', rows)


def echo():
    rows = []
    for line in (ROOT / 'aec-shipped-echo.txt').read_text().splitlines():
        if 'native-echo-pair,' not in line:
            continue
        case, measurements = line.split('native-echo-pair,', 1)[1].split(',current=', 1)
        values = dict(part.split('=', 1) for part in ('current=' + measurements).split(','))
        rows.append(dict(case_window=case, **{key: float(value) for key, value in values.items()}))
    assert len(rows) == 549
    write('aec-shipped-echo.csv', rows)
    base = sum(row['energy-current'] for row in rows)
    candidate = sum(row['energy-candidate'] for row in rows)
    summary = dict(windows=len(rows), over_1db=sum(row['delta'] > 1 for row in rows),
                   over_3db=sum(row['delta'] > 3 for row in rows), over_6db=sum(row['delta'] > 6 for row in rows),
                   worst_db=max(row['delta'] for row in rows), baseline_energy_fs2_s=base,
                   energy_fs2_s=candidate, total_energy_delta_db=10 * math.log10(candidate / base))
    assert [summary[key] for key in ['over_1db', 'over_3db', 'over_6db']] == [6, 2, 0]
    write('aec-shipped-echo-summary.csv', [summary])
    print('Echo:', summary)


def routes():
    rows = []
    means = []
    for line in (ROOT / 'aec-shipped-routes.txt').read_text().splitlines():
        if 'shipped-route\t' in line:
            scenario, far, delay, near, arm, index, value = line.split('shipped-route\t', 1)[1].split('\t')
            rows.append(dict(scenario=scenario, far_hz=int(far), left_delay_ms=int(delay), near_hz=int(near),
                             arm=arm, window_index=int(index), value=float(value)))
        elif line.startswith('shipped-route-mean\t'):
            scenario, arm, index, value = line.split('\t')[1:]
            means.append(dict(scenario=scenario, arm=arm, window_index=int(index), mean=float(value)))
    assert len(rows) == 27 * 6 * 17
    assert len(means) == 6 * 17
    write('aec-shipped-routes.csv', rows)
    write('aec-shipped-route-means.csv', means)
    lookup = {(row['scenario'], row['far_hz'], row['left_delay_ms'], row['near_hz'], row['window_index'], row['arm']): row['value'] for row in rows}
    for scenario in dict.fromkeys(row['scenario'] for row in rows):
        selected = [row for row in rows if row['scenario'] == scenario and row['arm'] == 'P7-unreported']
        echo_case = 'another room' in scenario
        for baseline in ['main-unreported', 'cold']:
            misses = []
            for row in selected:
                key = (scenario, row['far_hz'], row['left_delay_ms'], row['near_hz'], row['window_index'], baseline)
                delta = row['value'] - lookup[key]
                if (delta > 0 if echo_case else delta < 0):
                    misses.append(delta)
            print(scenario, baseline, 'misses', len(misses), '/', len(selected),
                  'worst delta', (max(misses) if echo_case else min(misses)) if misses else 0)


if __name__ == '__main__':
    speech()
    echo()
    routes()
