#!/usr/bin/env python3
"""Stop only a test's own processes on a new Playwright Chromium audio-out assertion.
Capture before launch: python3 scripts/e2e/guard-silent-chromium.py --capture-baseline BASELINE
Then launch the test, and: python3 scripts/e2e/guard-silent-chromium.py NODE_PID BASELINE
"""
import dataclasses
import datetime
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

@dataclasses.dataclass(frozen=True)
class Process:
    pid: int
    parent: int
    born: str
    command: str

ASSERTION = re.compile(r'^\s*pid\s*(\d+)\([^\n]*?\):\s*\[(0x[0-9a-f]+)\].*$', re.I)
PS = re.compile(r'^\s*(\d+)\s+(\d+)\s+(\w+\s+\w+\s+\d+\s+\d+:\d+:\d+\s+\d+)\s+(.*)$')

def assertions(text):
    result = []
    blocks = []
    current = None
    for line in text.splitlines():
        match = ASSERTION.match(line)
        if match:
            if current: blocks.append(current)
            current = (int(match[1]), match[2].lower(), [line.strip()])
        elif current and line[:1].isspace():
            current[2].append(line.strip())
        else:
            if current: blocks.append(current)
            current = None
    if current: blocks.append(current)
    for owner, key, lines in blocks:
        block = " | ".join(lines)
        if 'audio-out' not in block.lower(): continue
        # coreaudiod owns the assertion, but pmset names the actual audio client below it.
        client = re.search(r'Created for PID:\s*(\d+)', block)
        result.append((int(client[1]) if client else owner, key, block))
    return result

def table(text):
    rows = {}
    for line in text.splitlines():
        m = PS.match(line)
        if m:
            p = Process(int(m[1]), int(m[2]), m[3], m[4])
            rows[p.pid] = p
    return rows

def owner_chrome(p):
    return '/Applications/Google Chrome.app/' in p.command

def chromium(p):
    return not owner_chrome(p) and ('Chromium.app/' in p.command or 'chrome-headless-shell' in p.command or '/ms-playwright/' in p.command and 'chrome' in p.command.lower())

def discover(rows, known, browsers):
    # Birth times fence PID reuse, including a child reparented after its parent exited.
    owned = {pid for pid, born in known.items() if pid in rows and rows[pid].born == born and not owner_chrome(rows[pid])}
    changed = True
    while changed:
        changed = False
        for p in rows.values():
            if p.pid not in owned and p.parent in owned and not owner_chrome(p):
                owned.add(p.pid); known[p.pid] = p.born; changed = True
    changed = True
    while changed:
        changed = False
        for pid in owned:
            p = rows[pid]
            if pid not in browsers and (chromium(p) or p.parent in browsers and p.parent in rows and browsers[p.parent] == rows[p.parent].born):
                browsers[pid] = p.born; changed = True
    return owned

def matches(text, baseline, rows, browsers):
    return [(pid, line) for pid, key, line in assertions(text)
            if (pid, key) not in baseline and pid in browsers and pid in rows
            and rows[pid].born == browsers[pid] and not owner_chrome(rows[pid])]

def pmset():
    return subprocess.check_output(['pmset', '-g', 'assertions'], text=True)

def processes():
    return table(subprocess.check_output(['ps', '-axo', 'pid=,ppid=,lstart=,comm='], text=True))

def main(args):
    if len(args) == 2 and args[0] == '--capture-baseline':
        Path(args[1]).write_text(pmset())
        return 0
    if not 1 <= len(args) <= 2:
        raise SystemExit('Usage: guard-silent-chromium.py NODE_PID [BASELINE] | --capture-baseline BASELINE')
    root = int(args[0]); rows = processes()
    if root not in rows: return 0
    if owner_chrome(rows[root]): raise SystemExit('Refusing to guard the owner\'s Google Chrome')
    baseline_path = Path(args[1]) if len(args) == 2 else None
    baseline_text = baseline_path.read_text() if baseline_path else pmset()
    baseline = {(pid, key) for pid, key, _ in assertions(baseline_text)}
    known = {root: rows[root].born}; browsers = {}
    log = Path(str(baseline_path) + '.matches.log') if baseline_path else Path('guard-matched.log')
    while root in rows and rows[root].born == known[root]:
        owned = discover(rows, known, browsers)
        matched = matches(pmset(), baseline, rows, browsers)
        if matched:
            with log.open('a') as out:
                for pid, line in matched:
                    note = f'{datetime.datetime.now(datetime.timezone.utc).isoformat()} root={root} matched_pid={pid}: {line}'
                    print(note, file=out); print('GUARD: ' + note, file=sys.stderr)
            # No process-group signals: signal only exact processes this run started, after
            # checking their birth times again. Google Chrome is excluded a second time.
            current = processes()
            for pid in sorted(owned - {root}, reverse=True) + [root]:
                p = current.get(pid)
                if p and p.born == known[pid] and not owner_chrome(p):
                    try: os.kill(pid, signal.SIGTERM)
                    except ProcessLookupError: pass
            return 3
        time.sleep(0.3)
        rows = processes()
    return 0

if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
