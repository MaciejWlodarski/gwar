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
spacing between bursts and Gwar's socket cap remain enabled.

Regressions cover multi-packet burst draining, Game startup with scheduler
jitter, recovery after a bandwidth collapse, and 16-byte Text deltas at 5 fps.
The e2e screen tests use 6-byte packets. Remove these changes when upstream
provides equivalent protection. Changed upstream files:
`src/streams/send.rs`, `src/pacer/leaky.rs` and `src/bwe/probe/cluster.rs`.
