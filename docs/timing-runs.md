# Lip-sync timing on hosted runners

`.github/workflows/timing.yml` runs the lip-sync measurement (`apps/web/e2e/share-sync.mjs`, see
the "Lip-sync" section of [share-audio-validation.md](share-audio-validation.md)) as A/B runs on
GitHub's hosted Ubuntu runners, many at a time, instead of one after another on a Mac. It is manual
only: it never runs on a push or a pull request and cancels nothing in `ci.yml`.

The measured code comes from the `ref` input; the workflow's own scripts (`scripts/timing/`) come
from the commit it is dispatched from. So a branch whose lip-sync kit has not merged yet can be
measured from `main`'s workflow.

## Running it

```sh
gh workflow run timing.yml -f ref=screen/share-sound-sync \
  -f rows=j5,j10,steps,stall2s -f arms=off,on -f runs=8 -f viewers=browser,desktop
gh run watch "$(gh run list --workflow timing.yml --limit 1 --json databaseId -q '.[0].databaseId')"
gh run download <run id> -n timing-report   # table.md, results.json, plan.json, every run's record
```

The table is the run's job summary (the `aggregate` job). Each shard's artifact
(`timing-shard-N`) also holds every run's full share-sync report (`raw/*.json.gz`) and log.

| Input | Default | Meaning |
| --- | --- | --- |
| `ref` | (required) | Branch, tag or SHA measured. It must contain `apps/web/e2e/share-sync.mjs`. |
| `rows` | `j10` | Comma list of rows, below. |
| `arms` | `off,on` | Comma list of arms, below. The first is the baseline every other arm is compared with. |
| `runs` | `8` | Runs per arm per row. |
| `viewers` | `browser,desktop` | Who watches, side by side in every run. |
| `engine` | `chromium` | The viewers' engine; `webkit` is possible. The sharer is always Chromium. |
| `seconds` | row's own | Watch length per run, 10 to 300. |
| `net` | | A `LATENCY_NET` spec for the `custom` row. |
| `shards` | `20` | Parallel jobs, at most 20 (the free tier's limit). Normal CI queues behind them meanwhile. |
| `profile` | `release` | Cargo profile of the fixture and the native bridge. |
| `seed` | run id | Seed of the run order; the same seed and inputs give the same plan. |

Rows are the link in front of every viewer (`SYNC_NET`, `seed=42;down:` as in the
[screen-share jitter profiles](latency.md#profiles-and-measurement-rules)) and the run's shape:

| Row | `SYNC_NET` / switch | Watch |
| --- | --- | --- |
| `plain` | `seed=1` (no impairment) | 45 s |
| `j5`, `j10`, `j20` | `delay=20ms,jitter=5ms` / `10ms` / `20ms`, `keep_order` | 45 s |
| `r10` | `delay=20ms,jitter=10ms` (reordered) | 45 s |
| `h10` | `delay=20ms,jitter=10ms,hold=10ms,keep_order` | 45 s |
| `cap` | `delay=20ms,jitter=5ms,keep_order,rate=4mbit,queue=100ms` | 45 s |
| `steps` | `stall=270ms@10s,every=10s` | 45 s |
| `stall2s` | `stall=2s@14s` | 45 s |
| `return` | `seed=1`, `SYNC_RETURN=1` (sound stops 8 s in, returns 5 s later) | 30 s |
| `custom` | the `net` input | 45 s |

Arms: `off` (`SYNC_OFF=1`, main's playback), `on`, `nofilter` (`SYNC_NOFILTER=1`), `nohold`
(`SYNC_NOHOLD=1`), `nowait` (`SYNC_NOWAIT=1`). A new row or arm is one line in
`scripts/timing/lib.mjs`.

## What it does

1. `plan` checks the inputs and writes the plan: for every row and repetition one block that holds
   each arm once, in a seeded random order; the next repetition runs the reverse order, so every arm
   runs as often first as last. With two viewers their order flips every two repetitions. Blocks are
   dealt whole to the shards, so the arms of a block share one runner and its load at that time.
2. `native` builds `web_latency` (server, relay and network emulator) and `native_bridge` once.
   `wasm` takes the web app's two wasm packages from the measured commit's own CI run while it still
   has them (7 days), else from this workflow's cache or ci.yml's, else builds them as ci.yml does.
3. `measure`, one job per shard, runs its runs in order, as ci.yml's `browser-share-sync` does:
   PulseAudio with a null sink as the only output (nothing is audible), `SYNC_FAKE_TAB=1` (Linux
   Chromium captures a tab's picture but not its sound). Around every run it records the runner's
   CPU model and count, the 1-minute load before, after and its peak (sampled every 2 s), and the
   run's busy, I/O-wait and steal share of CPU time from `/proc/stat`.
4. `aggregate` builds the table from every shard's records, even when a shard failed.

A run is **invalid**, kept with its reasons and left out of the table, when share-sync failed or
timed out, when the 1-minute load peaked above 2 per CPU (`TIMING_MAX_LOAD_PER_CPU`; a valid run peaks
near 1 per CPU), when the
hypervisor took over 5% of the CPU time (`TIMING_MAX_STEAL`), or when the relay could not send
datagrams. The "Invalid and missing runs" table lists them.

## How long it takes

Measured in the validation run below: `plan` 12 s; `native` 5.5 min with a cold Rust cache
(less once this workflow's cache is warm); `wasm` 15 s when the commit's CI artifacts still exist
(a cold build of both packages takes about 15 minutes, in parallel with `native`); each shard about
1.5 minutes of setup (apt, pnpm, browsers); a run of 45 s about 57 s, of 30 s about 42 s. So
rows=4, arms=2, runs=8 (64 runs in 32 blocks, at most 2 blocks of 2 runs per shard over 20 shards)
takes about 12 to 15 minutes from dispatch to table, against about 1.5 hours in series on the Mac,
as long as 20 jobs can start at once.

## Reading the table

Per row, one line per viewer and arm:

- **Valid runs**: valid of reported.
- **First 12 s**: bursts in range of paired bursts, over every valid run, for the bursts drawn in
  the first 12 s after the viewer's watch started (when it was granted; else its first picture).
  In range is −45 to +125 ms, from the measured ref's `share-sync-judge.mjs`.
- **Settled**: the same for every later burst, then the median, p5 and p95 of those offsets
  (positive: sound behind the picture).
- **Per-run** columns: each valid run's share in range, sorted, so one bad run cannot hide in a
  pooled count.

Below each row, every arm against the baseline, per viewer and window, **over runs, not bursts**:
the bursts of one run share its start, its runner and its link, so they are not independent, and a
test over bursts would call nearly any difference significant. Δ is the difference of the pooled
shares in range in percentage points; the interval is a bootstrap that resamples whole runs within
each arm; p is a permutation test on the runs' own shares (exact up to 20,000 arrangements). "Better"
or "worse" needs the interval to exclude zero and p < 0.05. With 2 runs per arm the smallest
possible p is 0.33, so a small dispatch shows that the workflow works, not a difference; 8 runs per
arm can show a clear one.

The "Runner load by arm" table should show the same load for every arm; if it does not, distrust
the comparison.

## Noise: hosted runners are not the Mac

Hosted runners are shared 4-CPU virtual machines; their CPU model and neighbours vary from job to
job and day to day, and they are slower than an Apple M5. Absolute numbers differ from the Mac's
tables, and from one dispatch to the next. **Compare arms only within one dispatch**, where the
interleaving spreads each runner's load evenly over the arms. A result that matters belongs on the
Mac as well; this workflow says which comparisons are worth that.

What else differs from a local run: the shared tab is the sharer's page (`SYNC_FAKE_TAB`), not a
captured tab; Linux WebKit has no `RTCRtpScriptTransform`, so with `engine=webkit` the desktop viewer
shows the picture only and measures nothing; and `stall2s`'s stall is at 14 s on the fixture's
clock, which starts before the browsers connect, so on a slow runner it can fall early in the watch.
Check the viewers' `granted` against the stall in the raw reports when that row matters.

## Validation

The workflow was validated before it merged, from a temporary branch whose only extra commit gave it
a `push` trigger (a `workflow_dispatch` workflow can only be dispatched once it is on the default
branch), since deleted: [run 42015282056](https://github.com/MaciejWlodarski/gwar/actions/runs/42015282056),
`ref=screen/share-sound-sync` (`f987ed68`), `rows=j10`, `arms=off,on`, `runs=2`, `viewers=browser`,
2 shards, 9 min 42 s in all. All 4 runs were valid; the runners were an AMD EPYC 7763 and an EPYC
9V74, 4 CPUs each, with peak loads of 3.1 to 3.8, 43 to 55% busy and no steal. Two runs per arm show
that the pipeline works, not a result: the permutation p cannot go below 0.33 with them.
