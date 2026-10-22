# str0m 0.24.1: screen probe protection

`StreamTx::poll_packet_padding` uses blank padding instead of a cached packet
smaller than `MAX_BLANK_PADDING_PAYLOAD_SIZE` (240 bytes). NACK repair is
unchanged. Reusing tiny Text deltas amplified payload-sized probe clusters
with RTP/SRTP headers and exhausted the SFU's wire-byte reserve.

`LeakyBucketPacer` drains an already generated probe padding burst immediately,
including its timeout re-entry. The minimum probe delta separates bursts,
not each packet within a burst. Previously a 240-byte blank padding packet
required its own timer wake; even an unconstrained viewer could not bootstrap
past keyframe admission or recover after a pause. Application-paced
(`unpaced`) media also re-enters at the current clock, so
Gwar's released originals drain without a per-packet timer. `ProbeClusterState`
accounts for elapsed time on a late wake, within the remaining cluster bytes;
spacing between bursts and Gwar's socket cap remain enabled. Application-paced
probes account for the observed wake cadence in their byte target and first
burst, batch the minimum packet count at low rates, and space from actual send
time when padding is prepared before it is polled. Fallback probes inherit the
source's pacing mode. Paced streams retain the original first-burst sizing and
one-microsecond timeout re-entry, including during probes. The previous generic
same-clock timeout caused Gwar's `simple::bwe_cellular` regression; unchanged
crates.io str0m 0.24.1 was not affected.

`Bwe::set_min_probe_interval` optionally spaces dispatched clusters, including
the initial pair, and persists across estimator resets. Gwar watch transports
use one second during recovery or coarse scheduling to refill their extras
reserve. Fast active transports and the default retain zero.
`Bwe::request_recovery_probe` lets a paused application explicitly queue one
probe while loss-limited, respecting the same target and dispatch interval.
These requests preserve cluster IDs and estimator/feedback history during
discovery. Gwar rebases sparse congestion/ACK history once at measured capacity
when media resumes, retaining recovery spacing for ten seconds afterward.

`Bwe::set_padding_bitrate_limit` caps regular background padding independently
of probe clusters. The session reapplies the limit after estimate updates and
retains it across estimator resets. Gwar screen-watch transports set it to zero
while their reserve refills, then remove the limit when the reserve is ready.
This keeps str0m's 50 kbps idle padding from exhausting a probe reserve
refilling at only 10 kbps after a collapse. Probe traffic still passes Gwar's
actual-byte extras cap; its rate and bucket capacity are unchanged.

Regressions cover multi-packet burst draining, Game startup with scheduler
jitter, 70 combinations of collapse duration, cadence and downlink loss with
resume and sustained received video, preserved paced-media clock advancement,
the padding disable/reset/re-enable lifecycle, and 16-byte Text deltas at 5 fps.
The e2e screen tests use 6-byte packets. Remove these changes when upstream
provides equivalent protection. Changed upstream files:
`src/streams/send.rs`, `src/streams/mod.rs`, `src/pacer/leaky.rs`,
`src/bwe/probe/cluster.rs`, `src/bwe/probe/control.rs`, `src/bwe/mod.rs`,
`src/bwe/api.rs` and `src/session.rs`. Two test-only lint cleanups in
`src/packet/buffer_rx.rs` and `tests/change-ssrc-reset-receive.rs` allow the full
vendor clippy run with warnings denied.
