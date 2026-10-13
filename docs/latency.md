# Native mouth-to-ear latency

`vc-client`'s `latency` example feeds synthetic audio into one native client's
microphone ring and timestamps decoded marker onsets at another's speaker ring.
It includes capture batching, Opus, WebRTC, the Gwar SFU, jitter buffering,
decoding and virtual speaker playout. Both clients run on the measuring host and
share a monotonic clock, including with a remote server. Physical audio devices,
browsers, TeamSpeak and Gwar Connect are not used.

Virtual devices run 48 kHz mono with phased 480-sample (10 ms) callbacks. After
two seconds of warmup, a quiet 400 Hz carrier contains a 20 ms marker every
250 ms. Markers cycle through eight distinct tones: 900, 1100, 1300, 1500,
1700, 1900, 2300 and 2700 Hz, separated from the carrier's harmonics. A trailing
5 ms RMS window detects onset (on/off thresholds 0.18/0.11). A Hann-weighted
Goertzel filter over the next 10 ms of decoded audio identifies the tone. The
winning tone must contain at least 80% of the tone-bank power and explain at
least 60% of the weighted signal energy; ambiguous or incomplete onsets have
no identity and cannot match.

Each capture matches the first unused onset with the same tone identity within
the next eight marker periods (2000 ms, exclusive). An onset is consumed at most
once. Lost or ambiguous markers count as unmatched without shifting later
matches. The measurable range is **5 ms to below 2000 ms** by default. The
identities repeat every two seconds: actual latency at or beyond that limit
can alias to a previous cycle, so use this harness only where the total latency
is known to remain below two seconds. `LATENCY_MIN_MS` configures the positive
sanity floor; any candidate below it is recorded, consumed and fails the run,
even if at least 95% of other markers match.

Detector delay and callback batching are included without correction. Tone
classification uses samples after onset but keeps the original onset timestamp.
This approximates native mouth-to-ear latency; acoustic latency and physical
device timing need a separate measurement. The tones are exercised through the
native 48 kbps Opus pipeline in local end-to-end runs.

## Local runs

Use the native Rust prerequisites in the README and Python 3.9+ (stdlib only).
From the checkout root:

```sh
cargo build --release -p vc-client --example latency
cargo build --release -p vc-server
mkdir -p target/latency
cargo run --release -p vc-client --example latency -- target/latency/local-1.csv 100
cargo run --release -p vc-client --example latency -- target/latency/local-2.csv 100
python3 scripts/latency/run_local.py \
  --variant current=target/release/examples/latency \
  --markers 100 --runs 4 --out target/latency/current
```

Usage: `latency output.csv [marker-count]`; count defaults to 100 and must be
positive. Create the output parent first. Each local run uses a fresh server,
random loopback HTTP/UDP ports and a temporary data directory beside the CSV,
removed on completion, errors or Ctrl-C. Allow roughly `5 + 0.25 * count` seconds
plus setup. Unset `LATENCY_URL` and `LATENCY_SERVER` for local mode; the local
runner clears inherited `LATENCY_*` and `VC_*` settings itself. Both runners
accept `--minimum-latency-ms` (default 5) and `--extra-delay-ms` (default 0),
passing these settings explicitly to the client.

To check latency beyond one marker period, delay the audio itself in the fake
receiver output device:

```sh
LATENCY_EXTRA_DELAY_MS=300 target/release/examples/latency target/latency/delayed.csv 100
python3 scripts/latency/run_local.py \
  --variant current=target/release/examples/latency --extra-delay-ms 300 \
  --markers 100 --runs 1 --out target/latency/delayed
```

The zero-filled sample delay line holds every decoded sample before virtual
playout; it does not modify timestamps or the voice engine. Added delay must be
a nonnegative integer below 2000 ms, and **total** latency must still be below
the matching window. A 300 ms delay should add about 300 ms to the measured
latency, including when that moves the onset into a later marker period.
The run drains for an additional matching window after marker generation.

## Loopback network impairments

Set `LATENCY_NET` on a direct local invocation to insert a UDP relay between
both native clients and the fresh server. Signaling stays on its direct
WebSocket connection. Unset the variable for the original direct media path;
`LATENCY_NET=seed=1` exercises the relay without impairments. The relay rejects
existing/remote server mode. The comparison runners clear `LATENCY_*`, so invoke
the example directly for network experiments:

```sh
LATENCY_NET='seed=42;up:jitter=10ms' \
  target/release/examples/latency target/latency/up-jitter.csv 100
LATENCY_NET='seed=42;down:jitter=10ms' \
  target/release/examples/latency target/latency/down-jitter.csv 100
LATENCY_NET='up:stall=270ms@12s' \
  target/release/examples/latency target/latency/up-stall.csv 100
LATENCY_NET='down:stall=270ms@12s' \
  target/release/examples/latency target/latency/down-stall.csv 100
LATENCY_NET='seed=42;up:delay=20ms,jitter=5ms,keep_order,stall=270ms@12s,every=10s;down:loss=1%,burst=3@0.5%,reorder=0.5%' \
  target/release/examples/latency target/latency/mixed.csv 100
```

The spec consists of semicolon-separated `up:options`, `down:options`, and an
optional global `seed=N` section. Options use commas; each direction/key may
appear once. Omitted directions pass through. Durations require `ms` or `s`
and accept decimals (0 through 24 hours); percentages require `%` (0 through
100). Invalid specs fail before starting a server.

- `delay=20ms`: fixed delay on each surviving packet, default zero.
- `jitter=5ms`: signed uniform jitter in `[-5ms,+5ms]`, added to base delay;
  the total is clamped to zero. With no base delay, half of uniform draws pass
  immediately and half add up to 5 ms. `distribution=normal` uses a normal draw
  with sigma `jitter/3`, clipped to the same bounds; default is `uniform`.
- `keep_order` (or `keep_order=true`): hold a packet until the previous packet's
  scheduled release if needed. Default is false, so jitter may reorder packets.
- `stall=270ms@12s`: block egress from 12.000 through 12.270 seconds on the
  measurement clock, shared with `capture_s`/`playout_s`. The clock starts before
  voice setup; the first marker is around 2.2 seconds. Packets whose scheduled
  egress falls inside the window wait until its end, then leave as a burst,
  without pacing. Packets already in flight are included. `every=10s` repeats
  at 12, 22, 32… seconds; the period must exceed the stall duration.
- `loss=1%`: independent random packet loss, default zero.
- `burst=3@1%`: when outside a burst, each packet has a 1% chance of starting
  a loss of three consecutive packets, including the trigger. During the burst
  no new burst is started. Independent loss is applied to remaining packets.
- `reorder=0.5%`: independently add a hold to selected surviving packets so
  later packets can overtake them. `reorder_delay=20ms` sets that hold (default
  20 ms). Selection does not guarantee reordering if no packet overtakes it.
  Nonzero `reorder` conflicts with `keep_order`.
- `seed=42`: unsigned 64-bit seed, either global or inside a direction to
  override the global seed. The default global seed is 1.

Loss is applied first, then delay/jitter, the optional reorder hold, the stall,
and finally order preservation. The relay treats every UDP datagram alike,
including ICE/DTLS/RTCP; it never decrypts media. Severe impairments during
startup can prevent the voice connection. Each direction applies to **both**
clients, with independent random streams per client/direction. The server
advertises the relay's reserved loopback port using the existing
`Config.media_advertise`; its UDP bind stays on a different random port. `Running.media_bind` exposes
that actual socket address; `Running.media` remains the advertised address.
A separate upstream socket per client preserves ICE session routing and sends
server replies back from the advertised relay address. No production server
flags, defaults, or voice-engine behavior change.

SplitMix64 fixes the random algorithm across dependency upgrades. A seed
reproduces packet decisions and scheduled release times for the same packet
arrival trace. Real audio callbacks, OS UDP scheduling, ICE/control traffic and
actual release times still vary between runs; a seed does not make the full
mouth-to-ear measurement bit-for-bit deterministic. No packet is released
before its scheduled deadline. Each direction/flow has a 65,536-packet queue
limit; exceeding it or a socket/task error fails the run.

The run JSON's `network` is `null` without the relay. Otherwise it records the
original spec (`network.spec.source`), resolved settings and seeds, relay/server
addresses, flow count, and aggregate `up`/`down` counts:
`received`, `forwarded`, `delayed`, `dropped`, `reordered`, and `pending`.
`delayed` counts surviving packets scheduled later than arrival, once per
packet even if several impairments apply. `dropped` counts deliberate loss.
`reordered` counts a forwarded packet whose arrival sequence is below the
highest sequence already forwarded on its own flow; it does not count inversion
pairs or packets merely selected by `reorder`. `pending` counts packets still
queued at shutdown, separately from deliberate loss. With a completed relay,
`received = forwarded + dropped + pending` in each direction.

`stall_windows` lists windows that actually held packets, separately per flow
(first-arriving client is flow 0). Each contains planned `start_s`/`end_s`,
`first_packet_s` (arrival, possibly before the window for in-flight packets),
held `packets`, and measured `first_release_s`/`last_release_s` (`null` if none
were sent before shutdown). Resolved durations serialize as `{secs,nanos}`.
CSV/JSON retain the original matching and loss gates: an impairment run can
write useful output and still exit unsuccessfully if more than 5% of markers
are unmatched. Inspect per-marker CSV timelines and applied stall windows,
because a call's median alone can hide persistent latency after a stall.

On `a0f5686`, `up:stall=270ms@12s` left about +240 ms on every later marker
of the call (the SFU re-stamps slot RTP time from arrival, so the burst looks
like a new talk spurt), while `down:stall=270ms@12s` recovered to baseline
within one marker. `up:jitter=10ms` cost far more than `down:jitter=10ms`
(103 vs 63 ms median) for the same reason.

## Compare builds

Build each checkout into its **own `--target-dir`**: a shared directory can
silently overwrite one variant's executable with the other. Substitute absolute
checkout paths:

```sh
cargo build --manifest-path /path/to/baseline/Cargo.toml \
  --target-dir /path/to/baseline/target/latency-build --release -p vc-client --example latency
cargo build --manifest-path /path/to/candidate/Cargo.toml \
  --target-dir /path/to/candidate/target/latency-build --release -p vc-client --example latency
python3 scripts/latency/run_local.py \
  --variant baseline=/path/to/baseline/target/latency-build/release/examples/latency \
  --variant candidate=/path/to/candidate/target/latency-build/release/examples/latency \
  --markers 100 --runs 4 --out target/latency/comparison
```

Repeat `--variant name=path/to/client-binary` as needed. `--runs` means runs per
variant, default 4. Cycles alternate supplied and reversed order: two variants
with four cycles give A, B, B, A, A, B, B, A. Use an even cycle count for balance.
The first variant is the summary baseline. Avoid concurrent builds, tests or
measurements, and keep host/network conditions comparable.

## Remote server

Prepare an SSH-accessible checkout with a built `vc-server` and open the chosen
UDP port. The runner builds and installs nothing remotely. Choose unused ports
and supply all addresses and paths explicitly:

```sh
python3 scripts/latency/run_remote.py \
  --ssh-host user@your-host \
  --public-ip YOUR_PUBLIC_IP --media-bind YOUR_REMOTE_INTERFACE_IP \
  --media-port 9990 --http-port 8791 --tunnel-port 18791 \
  --remote-checkout /absolute/remote/checkout \
  --server-binary target/release/vc-server \
  --variant current=target/release/examples/latency \
  --markers 100 --runs 4 --out target/latency/remote
```

IPv4/IPv6 are accepted; the media bind IP may differ from the advertised public
IP behind NAT. Remote binary paths can be absolute or checkout-relative. To
interleave complete client/server builds, replace `--server-binary` with repeated
`--server-variant baseline=target/baseline/release/vc-server` and
`--server-variant candidate=target/candidate/release/vc-server`, supplying matching
client `--variant` names. Use separate target directories for server builds too.
Floor and artificial audio-delay flags are shared with the local runner; validation uses the
client's recorded identity window instead of assuming latency below 250 ms.

Each run starts a fresh server in a temporary directory under the remote
checkout's `target/`. Signaling binds loopback and uses an SSH tunnel; media
travels directly over UDP. Connect and TeamSpeak are disabled. Tunneled test
logins use `--insecure-any-host`; use dedicated test ports. SSH uses batch mode,
so authentication must work without interactive prompts. Remote startup uses
`;` separation and `< /dev/null` to detach the background process completely.
Cleanup stops only the recorded PID, collects its log, removes its directory and
stops the runner's tunnel. If connectivity prevents cleanup, the runner fails;
inspect the remote temporary directory's `pid` file before manual cleanup.

For an existing test server, invoke the example directly with both
`LATENCY_URL=ws(s)://host:port/ws` and `LATENCY_SERVER=IP:media-port` (bracket IPv6).
It stops only a server it started. Plain `ws://` must use loopback signaling,
such as an SSH tunnel; external signaling requires `wss://`.

## Output

The CSV contains matched marker index, tone identity, capture/playout seconds,
latency in ms and playout sample index. The adjacent `.csv.json` keeps requested
`markers`, matched `n`, lost marker indices, onset count, median/p95/p99,
receiver underflow samples/frame counters, voice events, addresses and detector
parameters. `marker_identity_count` is K; `matching_window_ms` records the
exclusive upper bound. `marker_captures` records capture time and identity for
every marker, indexed by marker number. `detected_onsets` records sample index,
playout time, identity (or `null`), confidence and tone energy fraction.
`ambiguous_onsets` and `unmatched_onsets` index that onset array;
`rejected_onsets` records below-floor candidates with marker/onset indices,
latency and reason. Floor, tone frequencies, classification thresholds and
artificial delay in both ms and samples are recorded too.

`receiver_playout` is the receiver's final live snapshot: speaker queue level,
latest and held maximum callback sizes, scheduling headroom, active-stream
underrun events and samples, and per-slot target/queue/staged samples,
concealment events, late drops, shrunk samples, `trim_events` and
`concealments_after_trim` (concealments within one second after a trim). A trim
event is one fractional correction or one stall-recovery batch; these counters
include both. Proximity is diagnostic correlation, not proof that a trim caused
the concealment. All sample counts use 48 kHz mono; divide by 48 for ms. These cumulative counters include warmup; raw
receiver underflows also include idle callbacks, whereas playout underruns count
only callbacks during playback.

Fractional corrections use the minimum queue + staged PCM margin observed
before refills over a full trailing five seconds. This retains sparse low-water
evidence such as a 5 ms packet delay every three seconds. The margin excludes
the pending refill, adaptive jitter reserve beyond packet batching, and 120 samples (2.5 ms)
of safety. Arrival phase contributes no credit. Each correction removes at most
240 samples, then rebuilds the observation window. Every concealment also
requires a fresh full window. Genuine surplus may take tens of seconds to
converge; the two-second nonquiet deadline cannot bypass the five-second
window, and speech onsets remain protected. Surplus below 120 samples is not
corrected.

For a variable output device, invoke the example directly with
`LATENCY_CALLBACK_JITTER=1 target/release/examples/latency output.csv 100`.
The receiver cycles through 480/960/240-sample callbacks (10/20/5 ms) on the same
sample clock; the sender stays at 480. JSON records `receiver_callback_pattern`.
The comparison runners clear `LATENCY_*` variables, so use direct invocation for
this mode.

Quantiles use empirical nearest rank, including the lower median for even n;
all-lost quantiles are `null`. CSV/JSON are written before rejecting below-floor
candidates or more than 5% lost markers.

Runners require a new `--out` directory and accept only ≥95% matched markers with
consistent CSV/JSON counts, one-to-one onset use, matching tone identities and
latencies within the recorded floor/window. Older binaries without identity
metadata are rejected; use the same updated harness in both comparison builds.
They never retry or exclude runs automatically.

Output includes per-run CSV/JSON/client logs, `manifest.json` (order, binary
SHA-256 hashes and metadata), combined `latency.csv`, and `summary.json` (pooled
quantiles and deltas from the first variant). Failed series record an error in
the manifest. Remote runs also retain server and tunnel logs.

Compare loss and underflows alongside latency. Frame counters cover warmup too.
Inspect individual call levels: startup buffering or network drift can dominate
pooled p95. The runners impose no fixed performance pass threshold.

## Native 10 ms frames

See [the 10 ms implementation report](opus-10ms.md) for transport compatibility,
FEC evidence, bandwidth budgets and the paired local baseline measurements.

## SFU media clock

Each receive slot maps the current source's 48 kHz media deltas, independent of
packet arrival spacing. Forward deltas up to five seconds preserve uplink
bursts and real silence, including packets omitted by the speech gate. Native
frames carry their first resampled capture position through the microphone ring
and outgoing queue; mute, PTT, VAD and queue drops still advance that position.
A ring overflow discards any partial frame spanning the missing samples.

The SFU uses RTP events for the actual SSRC, extended sequence number and raw
32-bit timestamp (MediaData has no SSRC). Timestamp wrap uses wrapping subtraction.
Duplicate timestamps and older sequences drop before speech-state updates;
a sequence restart requires a fresh SSRC or voice session. The last 32 retired
SSRCs cannot return.
TeamSpeak uses its wrapping u16 packet id times 960 as a synthetic clock,
with half-range ordering and separate voice/whisper identities; every listener
sees the same ids (checked against the official server in CI). The ids do not
advance across silence, so the bridge numbers each talk spurt (per speaker, and
per speaker and target for whispers) and a new spurt re-anchors the slot on wall
time, even for a listener that missed the end marker. An out-of-order id more
than a second after the last write starts a new clock instead of being dropped. Both bridge directions retain Opus durations.

First packets, speaker/SSRC changes, backward timestamps and deltas over five
seconds use elapsed wall time rounded up to the previous packet's duration,
at least one frame. Speaker reassignment also increments the existing slot
generation. Zero deltas drop: forwarding the same media instant would duplicate
playout. Dropping reordered packets keeps each output clock monotonic and lets
the receiver conceal loss without replaying stale audio. Very long genuine
pauses use the same wall-time fallback as a clock reset.

Local release comparison against `26bc30d` (three 100-marker runs per build):
baseline/candidate median 40.78/41.48 ms, p95 47.47/47.55 ms, p99 49.68/49.55 ms;
298/300 and 299/300 markers matched, respectively, with no receiver underflows.
No builds, tests or injected load ran during measurement; Spotlight remained
active, so these were not fully idle-host measurements. Real internet impairment,
live browser playback and official TeamSpeak interoperability remain unverified.
