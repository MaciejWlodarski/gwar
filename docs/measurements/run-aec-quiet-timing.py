"""Run the unchanged AEC six-pair test on pinned head/main in quiet alternating runs.

No audio device is opened. Each test uses virtual rings. Valid failures are kept;
only external-condition violations void an attempt. All attempts and conditions
are written before analysis. Uses a separate already-built binary per revision.
"""
import argparse
import datetime
import fcntl
import hashlib
import json
import os
import re
import signal
import subprocess
import time
from pathlib import Path

TEST = 'voice::codec_tests::four_shares_leave_the_voice_pipeline_on_time'


def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def external_processes(own_test=None):
    probes = {}
    for pattern in ['ss-tools/run.sh', r'e2e/share-sync\.mjs']:
        result = subprocess.run(['pgrep', '-f', pattern], capture_output=True, text=True)
        if result.returncode not in [0, 1]:
            raise RuntimeError('pgrep failed: ' + result.stderr)
        probes[pattern] = [int(value) for value in result.stdout.split()]
    processes = []
    table = subprocess.check_output(['ps', '-axo', 'pid,ppid,nice,comm'], text=True)
    for line in table.splitlines()[1:]:
        pid, parent, nice, command = line.strip().split(None, 3)
        pid = int(pid)
        name = Path(command).name
        rust_test = '/deps/' in command and re.search(r'-[0-9a-f]{16}$', name)
        if pid != own_test and (name in ['cargo', 'rustc'] or name.startswith(('gwar_client-', 'gwar_server-')) or rust_test):
            processes.append(dict(pid=pid, parent=int(parent), nice=int(nice), command=command))
    return dict(pgrep=probes, other_cargo_or_tests=processes)


def blocked(processes):
    return any(processes['pgrep'].values()) or bool(processes['other_cargo_or_tests'])


def output_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def execute_attempt(config, arm, number, output, start_limit, max_limit, deadline):
    prefix = output / f'{number:02d}-{arm}'
    before = dict(at=stamp(), load=os.getloadavg(), uptime=subprocess.check_output(['uptime'], text=True).strip(),
                  processes=external_processes())
    # Check again immediately before spawning; preflight waiting itself is not a run.
    if before['load'][0] >= start_limit or blocked(before['processes']):
        return None
    command = [config['binary'], TEST, '--exact', '--ignored', '--nocapture', '--test-threads=1']
    metadata = dict(arm=arm, sha=config['sha'], binary=config['binary'],
                    binary_sha256=hashlib.file_digest(open(config['binary'], 'rb'), 'sha256').hexdigest(),
                    command=command, before=before, start_nice=os.getpriority(os.PRIO_PROCESS, 0),
                    valid=False, invalid_reasons=[], start_limit=start_limit, max_limit=max_limit)
    # Hashing is completed before this final launch check, so no build/hash I/O overlaps timing.
    launch = dict(at=stamp(), load=os.getloadavg(), processes=external_processes())
    if launch['load'][0] >= start_limit or blocked(launch['processes']):
        return None
    metadata['launch'] = launch
    peak = launch['load'][0]
    with prefix.with_suffix('.txt').open('w') as log, prefix.with_suffix('.samples.jsonl').open('w') as samples:
        log.write('$ ' + ' '.join(command) + '\nRevision: ' + config['sha'] + '\nBefore: ' + before['uptime'] + '\n')
        log.flush()
        process = subprocess.Popen(command, cwd=config['cwd'], stdout=log, stderr=subprocess.STDOUT)
        metadata['pid'] = process.pid
        metadata['test_nice'] = os.getpriority(os.PRIO_PROCESS, process.pid)
        wall, active = time.time(), time.monotonic()
        last_process_check = 0.0
        try:
            while process.poll() is None:
                now_wall, now_active = time.time(), time.monotonic()
                loads = os.getloadavg()
                peak = max(peak, loads[0])
                sample = dict(at=stamp(), interval_s=now_active-active, load=loads)
                if now_active-last_process_check >= 5:
                    snapshot = external_processes(process.pid)
                    sample['processes'] = snapshot
                    if blocked(snapshot):
                        metadata['invalid_reasons'].append('overlap with another cargo/test or #213 timing')
                    last_process_check = now_active
                if loads[0] > max_limit:
                    metadata['invalid_reasons'].append(f'one-minute load exceeded {max_limit}')
                if abs((now_wall-wall)-(now_active-active)) > 2 or now_active-active > 3:
                    metadata['invalid_reasons'].append('sleep/clock gap or lost monitor coverage')
                samples.write(json.dumps(sample) + '\n')
                samples.flush()
                if deadline is not None and time.time() >= deadline:
                    metadata['invalid_reasons'].append('01:00 measurement deadline reached')
                if metadata['invalid_reasons']:
                    process.terminate()  # only this Popen child
                    break
                wall, active = now_wall, now_active
                time.sleep(1)
            try:
                metadata['exit_code'] = process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()  # only this Popen child
                metadata['exit_code'] = process.wait()
        except BaseException:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
            raise
    after = dict(at=stamp(), load=os.getloadavg(), uptime=subprocess.check_output(['uptime'], text=True).strip(),
                 processes=external_processes())
    peak = max(peak, after['load'][0])
    if after['load'][0] > max_limit:
        metadata['invalid_reasons'].append(f'one-minute load exceeded {max_limit} at end')
    if blocked(after['processes']):
        metadata['invalid_reasons'].append('overlap at end')
    metadata.update(after=after, peak_load_1m=peak, valid=not metadata['invalid_reasons'])
    metadata['pair_count'] = len(re.findall(r'pair \d+ order', prefix.with_suffix('.txt').read_text()))
    with prefix.with_suffix('.txt').open('a') as log:
        log.write('\nAfter: ' + after['uptime'] + '\nExternal-condition valid: ' + str(metadata['valid']) + '\n')
    output_json(prefix.with_suffix('.meta.json'), metadata)
    print(prefix.name, 'valid', metadata['valid'], 'exit', metadata['exit_code'], 'pairs', metadata['pair_count'],
          'load', round(before['load'][0], 3), '/', round(peak, 3), '/', round(after['load'][0], 3),
          metadata['invalid_reasons'], flush=True)
    return metadata


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binaries', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--resume', action='store_true')
    parser.add_argument('--start-load', type=float, default=3.5)
    parser.add_argument('--max-load', type=float, default=4.5)
    parser.add_argument('--deadline-utc')
    args = parser.parse_args()
    os.setpriority(os.PRIO_PROCESS, 0, 10)
    assert os.getpriority(os.PRIO_PROCESS, 0) == 10
    args.out.mkdir(exist_ok=args.resume)
    deadline = datetime.datetime.fromisoformat(args.deadline_utc).timestamp() if args.deadline_utc else None
    configs = json.loads(args.binaries.read_text())
    output_json(args.out / 'binaries.json', configs)
    accepted_path = args.out / 'accepted.json'
    accepted = json.loads(accepted_path.read_text()) if args.resume and accepted_path.exists() else []
    series = ['head', 'main'] * 3
    assert [run['arm'] for run in accepted] == series[:len(accepted)]
    existing_numbers = [int(path.name.split('-', 1)[0]) for path in args.out.iterdir() if path.name.split('-', 1)[0].isdigit()]
    attempt = max(existing_numbers, default=-1) + 1
    with open('/tmp/gwar-aec-quiet-223.lock', 'w') as lock, (args.out / 'preflight.jsonl').open('a' if args.resume else 'w') as preflight:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        lock.write(str(os.getpid()) + '\n');lock.flush()
        for arm in series[len(accepted):]:
            while True:
                if deadline is not None and time.time() >= deadline:
                    print('01:00 deadline reached; reporting the retained set so far.', flush=True)
                    return
                state = dict(at=stamp(), next_arm=arm, load=os.getloadavg(), processes=external_processes())
                preflight.write(json.dumps(state) + '\n');preflight.flush()
                if state['load'][0] < args.start_load and not blocked(state['processes']):
                    result = execute_attempt(configs[arm], arm, attempt, args.out, args.start_load, args.max_load, deadline)
                    if result is not None:
                        attempt += 1
                        if result['valid']:
                            accepted.append(result)
                            output_json(args.out / 'accepted.json', accepted)
                            break
                else:
                    print('WAIT', arm, 'load', round(state['load'][0], 3), 'blocked', bool(blocked(state['processes'])), flush=True)
                time.sleep(15)
    print('Completed six valid alternating runs; valid test failures have not been discarded.', flush=True)


if __name__ == '__main__':
    main()
