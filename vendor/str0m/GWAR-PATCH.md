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
probes with an explicit byte limit learn regular scheduling cadences up to 20 ms, batch the minimum packet
count at low rates, and space from actual send time when padding is prepared
before it is polled. Longer stalls never enlarge the retained cadence.
`Bwe::set_probe_byte_limit` bounds cluster payload bytes after the application
reserves worst-case RTP/SRTP headers and packet rounding. It survives resets.
A byte-limited first burst leaves a tail for a distinct send timestamp;
late wakes cannot exceed the remaining cluster budget. Fallback probes inherit the
source's pacing mode. Paced streams retain the original first-burst sizing and
one-microsecond timeout re-entry, including during probes. The previous generic
same-clock timeout caused Gwar's `simple::bwe_cellular` regression; unchanged
crates.io str0m 0.24.1 was not affected.

`Bwe::set_min_probe_interval` optionally spaces dispatched clusters, including
the initial pair, and persists across estimator resets. Gwar watch transports
use one second for every cluster, including fast active viewers and startup
pairs. The library default retains zero.
Fully paused viewers retry ordinary startup probing from 2.5 Mbps after 15 s
with no lane fitting, waiting for existing socket credit before restarting.
Continuous-fit timers and ready priority handoffs finish first. Resume starts fresh
feedback at the measured capacity after the ordinary continuous-fit guard.
`Bwe::reset` also resets smoothing, pending probe padding, the pacer and TWCC
send records, retaining transport sequence numbers and SRTP replay state.
Both the fallback source and every media/RTX stream discard pending padding;
queued media, repair caches and RTP/RTX sequence state survive.
Old feedback therefore cannot be attributed to new clusters with reused IDs.

When media starts a byte-limited unpaced cluster, its first padding request tops up that
same burst immediately, counting the media already sent. Previously its first
media bytes delayed padding until a later wake, making a healthy link look
slower and eventually causing false bandwidth re-pauses. Byte-limited first
bursts reserve a later timestamp even when media starts them. If released media
consumes that tail at the first clock, a byte-limited cluster stays open for a
later media packet or one blank packet within the existing MTU rounding reserve.
After a wake longer than the supported 20 ms regular cadence, it ends without
that packet and the zero-duration feedback is discarded, so the scheduler stall
is not measured as congestion. This prevents zero-duration feedback from stalling healthy, lossy bootstrap.
The guard applies only to explicitly byte-limited probes. Paced streams
retain their original first-burst timing and sizing. Unbounded transports retain
their original cadence handling and probe behavior too; the estimator's
saturation logic and thresholds are unchanged.

`Bwe::set_padding_bitrate_limit` caps regular background padding independently
of probe clusters. The session reapplies the limit after estimate updates and
retains it across estimator resets. Gwar screen-watch transports set it to zero
while their reserve refills, then remove the limit when the reserve is ready.
This keeps str0m's 50 kbps idle padding from exhausting a probe reserve
refilling at only 10 kbps after a collapse. Probe traffic still passes Gwar's
actual-byte extras cap; its rate and bucket capacity are unchanged.

Regressions cover multi-packet burst draining, Game startup with scheduler
jitter, 70 combinations of collapse duration, cadence and downlink loss with
resume and sustained received video, intermittent 60-250 ms scheduler stalls,
successive outages at 0-2% loss without re-pauses, clean estimator resets across
TWCC sequence wraparound, preserved paced-media clock advancement,
the padding disable/reset/re-enable lifecycle, and 16-byte Text deltas at 5 fps.
The e2e screen tests use 6-byte packets. Remove these changes when upstream
provides equivalent protection.

The BWE integration fixtures configure the RTCs and both wrappers with one
clock origin and 100 us resolution before connecting. They reach the existing
100 ms startup boundary by processing pending events, rather than assigning
wrapper clocks after the handshake and skipping or rewinding live timers.
The shared test driver opts BWE into scheduling each forced wake from the input
time it just handled, not the previous scheduled wake (which can still be in the future
when netem delivers a packet). Already-due timeouts advance from that input
clock too. Other integration fixtures retain their historical scheduling.
Aligning origins alone exposed a cellular failure; reverting alone
left capacity restoration intermittent. These fixture-only changes keep both
assertions and all rate thresholds unchanged.
Changed upstream files:
`src/streams/send.rs`, `src/streams/mod.rs`, `src/pacer/leaky.rs`,
`src/bwe/probe/cluster.rs`, `src/bwe/probe/control.rs`, `src/bwe/mod.rs`,
`src/bwe/api.rs`, `src/session.rs`, `tests/common.rs` and `tests/bwe/common.rs`. Two test-only lint cleanups in
`src/packet/buffer_rx.rs` and `tests/change-ssrc-reset-receive.rs` allow the full
vendor clippy run with warnings denied.

The seven canonical upstream PCAP test fixtures are included verbatim from
str0m 0.24.1, commit `8c4c5284cb11b1e3c82962639cda5b3e95af73e8`. They total
5,460,333 bytes and are redistributed under the preserved upstream MIT licence.
See `tests/data/README.md` for provenance and `tests/data/SHA256SUMS` for
checksums. The narrow `.gitignore` exception retains these required fixtures
so the entire vendor suite compiles and runs from a fresh checkout.


`Bwe::set_egress_pacing(media, probe)` opts a dedicated screen transport into a
shared RTP ceiling, including unpaced streams, RTX and probe padding. Debt uses
serialized SRTP plus a conservative 48-byte IPv6/UDP allowance, and TWCC and ALR
use that same byte unit. Rates and byte targets therefore describe occupied
capacity even for small padding. Queue pressure cannot increase this ceiling.
Ordinary bursts use 2 ms (one-MTU floor, one-packet rounding); regular wake
cadences up to 20 ms are allowed, but longer stalls do not create burst credit.
Probe configurations are capped before continuation thresholds are calculated,
and again at dispatch before the estimator and pacer receive them. Probes queue
a complete cluster for the shared pacer, avoiding two competing pacing clocks.
Coarse scheduling allows two regular wakes plus 2 ms for distinct timestamps,
within the existing byte reserve. Both rates survive BWE reset. Control traffic
bypasses the RTP pacer; transports that do not opt in retain upstream behavior.
Gwar enables this only on watch transports, at 90 % / 125 % of the current
estimate in steady state, allowing short 200 % probes during the 10 s
assignment/resume ramp and fully paused recovery. Independent voice transports keep their immediate path. Regressions
cover a burst of unpaced repairs, probes, transmission-time deadlines, and
5 Mbps downlinks at 0-1 % loss with both steady video and voice queue checks.
Additional changed upstream file: `src/pacer/mod.rs`.


`LossController::set_bandwidth_estimate` also replaces the loss-limited state
with `DelayBased`, matching WebRTC `LossBasedBweV2::SetBandwidthEstimate`.
Previously a successful probe could install a new estimate while retaining
`Decreasing`; the old HOLD cap could then overwrite demonstrated capacity.
An induced-loss regression verifies this transition. Loss thresholds and the
inherent-loss model are unchanged. Additional changed file:
`src/bwe/loss_controller.rs`.

Shared-pacing probe configurations also limit gaps between local sends to 20 ms.
The estimator retains ordered send timestamps (including known lost packets)
and rejects a cluster interrupted by a longer scheduler stall. A paced probe
spanning a 100 ms callback stall must not report that low send rate as network
capacity. Regular local sends that queue downstream still produce a congestion
estimate. Tests distinguish both paths; ordinary unconfigured transports retain
their probe behavior. Additional changed file: `src/bwe/probe/estimator.rs`.
