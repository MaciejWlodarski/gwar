# Native audio thread priority

The dedicated `vc-voice-pipeline` thread requests scheduling priority once at
startup. Promotion is best-effort: a refusal emits one debug message and the
pipeline continues. A thread-affine RAII guard restores the original scheduler
on normal exit, an error, or unwinding. Tokio workers and device enumeration
keep their existing scheduling. The guard lives in the shared pipeline, so it
also covers direct TeamSpeak mode without changing its 20 ms capture framing;
mixing and native playout still use `voice::FRAME` (480 samples, 48 kHz).

## Choice

- macOS: `THREAD_TIME_CONSTRAINT_POLICY`, period/deadline 10 ms, computation
  budget 5 ms, preemptible. Mach absolute clock conversion uses integer
  arithmetic. XNU clamps computation to at least half the constraint. Cleanup
  restores the old time-constraint policy if one existed, otherwise the saved
  extended timeshare/fixed policy; setting default RT constraints would not
  select the original scheduler. CoreAudio workgroup integration needs an
  actual AudioUnit workgroup/clock and more device lifecycle handling, so this
  portable frame clock is the smaller initial change.
- Windows: `AvSetMmThreadCharacteristicsW("Pro Audio")` and
  `AvRevertMmThreadCharacteristics`. MMCSS supplies the multimedia scheduling
  budget; no process priority change or fixed time-critical thread priority is
  requested for the pipeline.
- Linux: `pthread_setschedparam(SCHED_FIFO | SCHED_RESET_ON_FORK, priority=10)`.
  Permission comes from an existing `RLIMIT_RTPRIO` of at least 10 or
  `CAP_SYS_NICE`. A refusal keeps normal scheduling. No synchronous RTKit/D-Bus
  call, new system library or process-wide `RLIMIT_RTTIME` change is needed.
  Cleanup restores the old policy/priority and keeps `RESET_ON_FORK`, which an
  unprivileged thread cannot clear after promotion.

The small implementation uses thin bindings already in the dependency graph:
`libc` and `windows-sys` (MIT OR Apache-2.0), `mach2` 0.6
(BSD-2-Clause OR MIT OR Apache-2.0). These are maintained bindings from Rust,
Microsoft and the Mach bindings project, rather than a new scheduler abstraction.

Mozilla's `audio_thread_priority` is maintained (published 0.35.1 and 0.38.0
were inspected) and MPL-2.0. It is used indirectly by CPAL's `realtime` feature,
with its default D-Bus feature disabled. CPAL 0.18.2 requires 0.35, whose Linux
no-D-Bus implementation is a successful no-op. Version 0.38 adds direct Linux
FIFO support, but the Windows source still registers "Audio" despite the
current crate-level docs saying "Pro Audio". Its macOS restoration sets saved
time constraints even if those were the default policy. Direct bindings avoid
those mismatches for the pipeline. The lockfile adds CPAL's
`audio_thread_priority` 0.35.1 and its target-specific `mach2` 0.4.3 dependency;
there is no D-Bus dependency. No copied or modified MPL source is included.

## CPAL callbacks

The locked CPAL version is 0.18.2. Its feature is named `realtime`, not
`audio_thread_priority`; `realtime-dbus` additionally enables RTKit over D-Bus.

- CoreAudio: callbacks are invoked by the AudioUnit's OS-managed audio thread;
  CPAL does not create a normal Rust callback worker to promote. Leave those
  callbacks to CoreAudio.
- WASAPI: CPAL creates a worker per stream. With `realtime`, it requests MMCSS
  "Audio", with `THREAD_PRIORITY_TIME_CRITICAL` fallback on registration
  failure. This feature is now enabled. Failure is reported to the existing
  stream error callback; it does not terminate the stream.
- ALSA: a dedicated worker requests promotion only for eligible hardware/linear
  PCM types when `realtime` is enabled. Server-backed/possibly spinning ALSA
  plugins are deliberately excluded. With CPAL's 0.35 dependency and no D-Bus,
  the request is a no-op. Promoting every callback in application code would
  bypass that backend safety check, so this change preserves it.
- Native PipeWire: CPAL's local-listener callbacks run on its mainloop thread;
  it explicitly omits `RT_PROCESS`. With `realtime`, promotion requests go to
  a background worker, keeping D-Bus off the callback path. The no-D-Bus
  promotion is a no-op in this locked release. The Gwar
  build does not enable the optional native `pipewire` backend, so a typical
  PipeWire desktop currently reaches us through an ALSA plugin instead.

Linux CI and release already install ALSA/Opus and the desktop prerequisites.
`realtime` uses the existing ALSA bindings; neither workflow needs a new system
package. The change does not grant permissions or require root.

## Sources

Source inspection used the installed, checksum-verified registry packages.

- [CPAL 0.18.2 manifest](https://github.com/RustAudio/cpal/blob/e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7/Cargo.toml),
  [WASAPI promotion](https://github.com/RustAudio/cpal/blob/e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7/src/host/wasapi/stream.rs),
  [ALSA eligibility/promotion](https://github.com/RustAudio/cpal/blob/e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7/src/host/alsa/mod.rs),
  [PipeWire promotion](https://github.com/RustAudio/cpal/blob/e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7/src/host/pipewire/rt_promote.rs).
- [Mozilla audio_thread_priority](https://github.com/mozilla/audio_thread_priority),
  [0.35.1 platform dispatch](https://github.com/mozilla/audio_thread_priority/blob/60a5050838b10b1e5a5a30c49bcd0d0cdee935dc/src/lib.rs).
- [Apple Mach scheduling](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/KernelProgramming/scheduler/scheduler.html),
  [XNU policy switching and bounds](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/thread_policy.c).
- [Linux scheduling and permissions](https://man7.org/linux/man-pages/man7/sched.7.html).
- [Microsoft MMCSS](https://learn.microsoft.com/en-us/windows/win32/procthread/multimedia-class-scheduler-service),
  [task registration](https://learn.microsoft.com/en-us/windows/win32/api/avrt/nf-avrt-avsetmmthreadcharacteristicsw).

## Local evidence, 2026-10-14

macOS 27.0, arm64, 10 logical CPUs, 16 GiB RAM. Baseline was built at
`bdaac83eb54f75d1872d4a3dcc2d36b69157118f` before wiring in the guard; the
candidate uses the same harness and unchanged jitter/playout implementation.
Both builds used `cargo build --release -p vc-client --example latency -p vc-server`
and the worktree's own target directory.

The explicit macOS OS-policy test passed:

```text
vc-voice-pipeline: before=None
promoted=thread_time_constraint_policy { period: 240000, computation: 120000,
                                        constraint: 240000, preemptible: 1 }
period_ns=10000000
vc-voice-pipeline: restored=None
```

Live `ps -M -p <latency-pid>` snapshots five seconds into every measurement
also showed baseline threads at `31T` and the candidate's two pipeline threads
at `97R`. This demonstrates an actual scheduling-class change, not just an API
success result. The other candidate threads retained `31T`.

Ten separately spawned Python processes ran `while True: pass` throughout four
pairs, each baseline immediately followed by candidate. Fixed and jitter
pairs alternated; there were no concurrent builds/tests from this task.
Jitter mode used `LATENCY_CALLBACK_JITTER=1` (480/960/240-sample output callbacks).
All runs used fresh local loopback servers, 100 markers, no network impairment,
no extra delay, and matched 100/100 markers with successful exits. No remote
server or physical audio device was used. All ten busy-loop processes were
terminated by their own PIDs and waited for; read-back confirmed none remained.
Other agents shared the Mac, so their load and OS scheduling remain uncontrolled.

Raw per-run data (latencies in ms; headroom is the final live snapshot):

```csv
run,matched,median_ms,p95_ms,p99_ms,underruns,underrun_samples,headroom_samples,headroom_ms
1-baseline-fixed,100,57.564,79.570,82.411,2,960,960,20.0
1-candidate-fixed,100,47.671,51.291,52.281,0,0,480,10.0
2-baseline-jitter,100,77.094,80.613,81.846,0,0,960,20.0
2-candidate-jitter,100,84.417,90.093,91.509,0,0,960,20.0
3-baseline-fixed,100,56.670,60.743,62.223,0,0,480,10.0
3-candidate-fixed,100,47.305,50.636,51.537,0,0,480,10.0
4-baseline-jitter,100,80.347,89.532,92.535,0,0,960,20.0
4-candidate-jitter,100,76.705,81.020,81.776,0,0,960,20.0
```

The candidate had zero underruns throughout; baseline had two (960 samples) in
one fixed run, zero in the others. Fixed-callback final headroom was 10 ms for
both candidate runs, versus 20/10 ms for baseline. Jitter final headroom was
20 ms in both versions. Fixed medians fell by 9.893/9.365 ms; jitter medians
changed by +7.323/-3.643 ms. This supports successful promotion and a useful
fixed-callback observation under this load, not a consistent jitter-mode or
general latency improvement. Counters include warmup. Virtual callback threads
are unchanged, so these measurements do not validate CPAL callback priority or
physical mouth-to-ear latency.

Raw CSV/JSON, logs, thread snapshots, binary hashes, run order, load PIDs and
cleanup results remain local under
`/Users/zireael/dev/projects/gwar-p3-rtprio/target/latency/rtprio/`.
Binary SHA-256:

```text
baseline: 8bebe274913c308abe9cf3dac10c682c1cd58b288555752e8f08cf158058daf3
candidate: ce252293223c3730925546161787afbdc76d7da6fe8030009712d31435b1ad0c
```

To repeat using the retained baseline/candidate binaries and own-PID load
controller (writes the same local output filenames):

```sh
python3 /Users/zireael/dev/projects/gwar-p3-rtprio/target/latency/rtprio/compare.py
```

To recheck the actual Mac policy without audio devices or server connections:

```sh
CARGO_TARGET_DIR=/Users/zireael/dev/projects/gwar-p3-rtprio/target \
  cargo test --manifest-path /Users/zireael/dev/projects/gwar-p3-rtprio/Cargo.toml \
  -p vc-client --lib macos_policy_changes_and_restores -- --ignored --nocapture
```

Validation passed: `cargo fmt --all`, full-workspace
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p vc-client`
with `VC_TS3_DIR` unset, and a linked `cargo build -p vc-desktop` on this Mac.
The manual policy test is ignored by default because CI/sandboxes may refuse RT
scheduling. Official TeamSpeak-server tests were skipped via their existing
environment gate. Windows and Linux versions of the exact new module were
also type-checked and linted with their real target bindings in an isolated
crate under this worktree's target directory; that is not a complete desktop
build or runtime test on those OSes. CI was not run remotely.

`cargo audit` passed before and after with the same three existing allowed
warnings (`audiopus_sys`, `proc-macro-error`, `glib`). No cargo-deny configuration
or executable was present in this checkout/environment. No new system packages
or workflow changes were needed. Windows/Linux promotion and refusal handling,
Linux backend callback elevation, macOS audio workgroups and physical device
latency remain unmeasured here.
