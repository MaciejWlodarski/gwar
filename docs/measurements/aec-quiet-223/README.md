# Paused quiet AEC timing, PR #223

The owner paused work for 1–2 days on 2026-11-02. No waiting process, timer or
scheduled resumption remains. The former 01:00 deadline is cancelled by this pause.

No timing attempt started. `preflight.jsonl` contains only eligibility checks;
there are zero accepted runs and zero per-pair measurements. The recorder was
stopped while waiting, before any test child existed. `coordinator.txt` also
records the earlier pause for deterministic DSP checks and the threshold revision.

The prepared binaries were separate release builds of P7 at
`ac4af9e4c2f6a5dffee325f791b6835623f6ca49` and main at
`208c886668b597a1c1334c926bb8ccd2948e8b83`. `test-identity.json` proves that the
six-pair test and all three pipeline helpers have identical source on both.
Later changes only fix route-report iteration and record validation; production
remains the owner's Left warm P7 choice. All local build targets and the baseline
scratch worktree were deleted after the checkpoint. `binaries.json` records the
old paths for custody, not reusable binaries.

## Resume protocol

Build the resumed head and pinned main into separate targets, one heavy task at a
time, actual nice 10 and four Cargo jobs. Before starting a build, check other
cargo/test processes and `pgrep -f ss-tools/run.sh`. Do not overlap #213 timing.

Run head/main/head/main/head/main, three completed valid runs per revision, each
using the unchanged six-pair test. Start only at unrounded one-minute load <3.5;
void an attempt if it exceeds 4.5 at any sampled point, another cargo/test or
#213 timing overlaps, or sleep/lost monitor coverage occurs. Record uptime before
and after and load throughout. Keep every attempt and every valid failure.

Pass requires P7's max delta <=1.0 ms in the quiet runs and no systematic
worsening against main under the same conditions. Report all 36 pairs, each
run's load range, and the three adjacent head/main comparisons. Do not use the
historical uncontrolled +1.241 ms failure or +0.344 ms pass as acceptance.

Exact build, timing and remaining-validation commands are in the current P7
section of `../../share-audio-validation.md` and the PR's State / next steps.
The recorder accepts `--deadline-utc` if the owner provides a new deadline;
never reuse the cancelled 2026-11-03 deadline after this pause.
