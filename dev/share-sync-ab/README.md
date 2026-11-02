# DEV ONLY: #213 lip-sync measurement kit

Not product code and not run by CI. Tools and raw numbers for resuming #213
(share sound in sync with the picture) after a pause. Drop this directory before
merging #213 unless the owner wants to keep it.

## Build the fixtures once

```sh
CARGO_TARGET_DIR=/tmp/ss213-target CARGO_BUILD_JOBS=4 nice -n 10 \
  cargo build --release -p gwar-client --no-default-features --example web_latency --example native_bridge
```

## Rules for every timing run (owner's)

- Silence: Chromium only, which runs with `--disable-audio-output` on macOS. No local WebKit.
  `guard.py` kills a run only on a `pmset` audio-out assertion from that run's own process
  tree (never /Applications/Google Chrome.app), ignores assertions that were there before, and
  logs the matched line.
- Load: `run.sh` writes `<run>.meta` with the 1-min load at start and end. `valid.py` voids a
  run if either is above 6, or if `pmset -g log` shows a sleep in between. `sync.sh` waits for
  load ≤ 5, and for no `ss-tools/run.sh` (the other worker), before each run. It re-runs voids
  up to 3 passes.
- No heavy builds while a table runs (they void runs).
- CI can't time this row. ubuntu Chromium gives no tab sound, and macos-15 is a 3-core VM at
  load ~50. `ci-attempt/` is kept for the record only. Linux CI's `browser-share-sync` uses
  `SYNC_FAKE_TAB=1` (the sharer's page is its own source) as a functional check, not timing.

## Commands

```sh
# one row: name, net, seconds, run numbers from..to, arms (on off nohold nofilter neither)
dev/share-sync-ab/sync.sh j5b 'seed=42;down:delay=20ms,jitter=5ms,keep_order' 45 1 8 on off nohold nofilter neither
python3 dev/share-sync-ab/sync.py /tmp/ss213/data/j5b   # first 12 s, settled, median/p5/p95, back
```

| Row | net | s | extra env |
| --- | --- | --- | --- |
| ±5 ms | `seed=42;down:delay=20ms,jitter=5ms,keep_order` | 45 | |
| ±10 ms | `seed=42;down:delay=20ms,jitter=10ms,keep_order` | 45 | |
| 2 s break | `seed=42;down:stall=2s@14s` (the docs row "2 s stall at 14 s"; spec not recorded, verify) | 45 | |
| 270 ms breaks | `seed=42;down:stall=270ms@10s,every=10s` | 45 | |
| sound stopped and returned | `normal` | 30 | `ROW_ENV="SYNC_RETURN=1"` |

In range means −45…+125 ms. "Settled" means after the first 12 s of a viewer's record.
Arms: `off` = main's playback (`SYNC_OFF`), `nohold` = no start sound hold, `nofilter` =
no late-frame filter, `neither` = both left out.

Drop-sweep noise controls: `controls.sh` (see its header). Compare with
`sweepcmp.py` (per-case deltas) and `sweepdist.py` (value distributions); `sweepsub.py`
cuts out the old 80-case CI subset.

## Contents

- `results/*.txt`: summaries quoted in the PR body. `results/raw.tar.xz` has every run's
  JSON and meta, plus the 11 sweep files.
- `ss-tools-221/`: #221's jitter-design patches (verify-design, jitter-full) from the shared
  `/tmp/ss-tools`, kept here so they don't live only in /tmp. They are the jitter worker's,
  not #213's.
