# Validation evidence captured before the pause

These JSON files preserve the measurements needed to resume #222 after temporary
logs/builds are removed. They are observations, not blanket acceptance. Run links,
source pins, commands and limits are in [the handoff](../../denoise-preparation-handoff.md).
No result was discarded to make a workflow green.

- `denoise-prepared-initial-summary.json`: run 41519231198, three runners,
  pre-adaptive-cadence candidate `99f066d7`, main `208c8866`; WebKit warm trials
  are invalid because both variants lacked a fresh join gesture.
- `denoise-paired-final-summary.json`: run 41521763386, three runners,
  diagnostic `a51e1531`, production source `2a8be8a8`, main `3e3cbdce`;
  fresh join gestures and cold preparation inside the timer. Includes every
  readiness result, quality record and reported failure. The cold WebKit
  regression remains unaccepted.
- `pause-ci-state.json`: exact completed normal/paired job IDs, conclusions,
  failed steps and links. Normal screen failure was decoded-frame visibility;
  no rerun was requested during the pause.
- `denoise-c3-summaries.json`: original 10-before/10-after C3 traces, including
  tab/canvas variants. Full stress workflows were red on both sides.
- `denoise-call-final-summary.json`, `denoise-call-warm-final-summary.json`,
  `denoise-early-output-summary.json`, `denoise-render-clock-summary.json`:
  earlier rejected experiments, retained as context rather than acceptance.
- `mono-literal-summary.json`: #228's 20-original/20-fixed full-model WebKit
  measurements; DSP-free phase/power reproduction has its separate linked run.
- `history-readers-proof-summary.json`: #227's loaded old/fixed older-history
  results, with the unchanged five-second assertion.
- `mac-stall-correlation-summary.json`: aligned CPU/stall/native audio windows
  supporting the owner-accepted macos-15 VM limit. The permanent explanation is
  in #229, `docs/media-runner-limits.md`.
- `retired-diagnostic-branches.json`: previously deleted diagnostic branch pins.
  Active unfinished diagnostic branches are intentionally retained for resume.

Full raw CI logs/traces remain attached to their linked GitHub runs. Redundant
local copies and superseded local templates are disposable; current measurement
sources, the PID-scoped local guard and the log extraction command are committed.
