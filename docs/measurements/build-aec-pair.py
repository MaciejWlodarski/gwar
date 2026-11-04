"""Build pinned head/main test binaries sequentially in separate targets; no playback."""
import argparse
import json
import os
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--head-dir', type=Path, required=True)
    parser.add_argument('--main-dir', type=Path, required=True)
    parser.add_argument('--main-sha', required=True)
    parser.add_argument('--target-root', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    # Set an absolute priority: an inherited nice 10 must not become nice 20.
    os.setpriority(os.PRIO_PROCESS, 0, 10)
    assert os.getpriority(os.PRIO_PROCESS, 0) == 10
    args.target_root.mkdir(parents=True, exist_ok=True)
    results = {}
    for arm, source in [('head', args.head_dir), ('main', args.main_dir)]:
        source = source.resolve()
        sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
        assert not subprocess.check_output(['git', 'status', '--porcelain'], cwd=source, text=True).strip(), source
        if arm == 'main':
            assert sha == args.main_sha, (sha, args.main_sha)
        target = (args.target_root / arm).resolve()
        env = os.environ | {'CARGO_TARGET_DIR': str(target), 'CARGO_BUILD_JOBS': '4'}
        command = ['cargo', 'test', '--locked', '--release', '-p', 'gwar-client', '--lib', '--no-run', '--message-format=json']
        print('Building', arm, sha, 'actual nice 10; ensure no other heavy build or #213 timing.', flush=True)
        with (args.target_root / f'build-{arm}.jsonl').open('w') as stdout, (args.target_root / f'build-{arm}.log').open('w') as stderr:
            subprocess.run(command, cwd=source, env=env, stdout=stdout, stderr=stderr, check=True)
        events = [json.loads(line) for line in (args.target_root / f'build-{arm}.jsonl').read_text().splitlines() if line.startswith('{')]
        binaries = [event['executable'] for event in events if event.get('reason') == 'compiler-artifact'
                    and event.get('executable') and event['target']['name'] == 'gwar_client']
        assert len(binaries) == 1, binaries
        results[arm] = dict(sha=sha, cwd=str(source), binary=binaries[0], target=str(target))
        args.out.write_text(json.dumps(results, indent=2) + '\n')
        print('Built', arm, binaries[0], flush=True)


if __name__ == '__main__':
    main()
