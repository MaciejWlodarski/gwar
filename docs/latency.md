# Native mouth-to-ear latency

`gwar-client`'s `latency` example feeds synthetic audio into one native client's
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
cargo build --release -p gwar-client --example latency
cargo build --release -p gwar-server
mkdir -p target/latency
cargo run --release -p gwar-client --example latency -- target/latency/local-1.csv 100
cargo run --release -p gwar-client --example latency -- target/latency/local-2.csv 100
python3 scripts/latency/run_local.py \
  --variant current=target/release/examples/latency \
  --markers 100 --runs 4 --out target/latency/current
```

Usage: `latency output.csv [marker-count]`; count defaults to 100 and must be
positive. Create the output parent first. Each local run uses a fresh server,
random loopback HTTP/UDP ports and a temporary data directory beside the CSV,
removed on completion, errors or Ctrl-C. Allow roughly `5 + 0.25 * count` seconds
plus setup. Unset `LATENCY_URL` and `LATENCY_SERVER` for local mode; the local
runner clears inherited `LATENCY_*` and `GWAR_*` settings itself. Both runners
accept `--minimum-latency-ms` (default 5) and `--extra-delay-ms` (default 0),
passing these settings explicitly to the client.

`LATENCY_WAIT_CONNECTED=1` waits (at most 30 seconds) for both native transports
before arming the two-second marker warmup. Carrier audio, capture clocks and
network impairments still start at the original epoch; no voice queues are
reset. This separates connected playout loss from audio captured while ICE/DTLS
is unavailable. JSON records `wait_connected` and `connected_s`; capture and
playout timestamps retain the original epoch. The default remains the original
fixed warmup, which includes connection-setup losses. Use the same mode and
harness source in both builds. The runners clear this variable too, so set it
in executable wrappers when comparing variants.

`LATENCY_WARMUP_MS` replaces the two-second marker warmup (the run is extended
by the same amount; JSON `warmup_samples` records it). Without it, the receiver
is still trimming its startup depth while markers play. Idle runs then settle
at run-level p95 values around 40, 47 or 50–55 ms, depending on how much was
trimmed, which a 2 ms paired gate cannot resolve. With
`LATENCY_WAIT_CONNECTED=1 LATENCY_WARMUP_MS=15000`, idle runs cluster at about
42–45 ms, and the idle comparison resolves (docs/voice-sfu-5b.md). Gates use
15 s. Set it in the wrappers of both variants. The value is whole milliseconds,
at most 30000 (the runners' client deadline allows for that); anything else
fails the run. Impairment times such as
`stall=…@12s` stay on the measurement clock, so a longer warmup can move them
before the first marker: with 15 s the markers span about 15.2–40 s, and a
stall belongs at for example `@27s`.

`LATENCY_SFRAME=1` measures an encrypted call on the fresh local server. The
sender creates an encrypted channel, its channel MLS group and the call's
voice group, and the receiver joins both by external commit. Both native
engines start encrypted and install the epoch's MLS exporter keys through the
engine's key channel, the production path. The JSON records `sframe` and the
receiver's SFrame counters. Plaintext and SFrame then compare in one build:
set `LATENCY_SFRAME` to 0 in one variant's wrapper and to 1 in the other's.

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

## Pipeline scheduling

The dedicated voice pipeline requests best-effort real-time scheduling for its
480-sample/48 kHz (10 ms) work: Mach time constraints on macOS and MMCSS
"Pro Audio" on Windows. Both have an OS backstop for a thread that stops
blocking (XNU demotes it after overrunning its computation budget; MMCSS keeps a
share for ordinary threads). Linux stays timeshare: `SCHED_FIFO` has no such
backstop beyond global RT throttling, and an always-ready capture device (ALSA
`null`) would let the pipeline hold a core. Each pipeline pass also takes only
the capture samples queued when its capture drain starts and at most 256
incoming packets, so a producer faster than real time cannot starve playback. The guard restores the old policy on exit;
refused promotion logs once at debug level and voice continues. CPAL's
`realtime` feature also enables WASAPI callback promotion; CoreAudio owns its RT
callbacks. Mozilla's `audio_thread_priority` (used by CPAL off macOS) would add
an older `mach2` on macOS, so the pipeline binds the OS calls directly. macOS
audio workgroups are not joined yet.

Local check (2026-10-14, Mac under ten busy loops, four baseline/candidate pairs,
100/100 markers each): the pipeline thread moved from `31T` to `97R` in
`ps -M`. Fixed callbacks: median 57.6/56.7 → 47.7/47.3 ms, underruns 2/0 → 0/0,
final headroom 20/10 → 10/10 ms. With `LATENCY_CALLBACK_JITTER=1` the medians
moved +7.3/−3.6 ms (noise), so the gain is fewer scheduling misses under load,
not a general latency cut. The harness's virtual callbacks do not exercise CPAL
callback priority; Windows promotion is untested at runtime.

Compare unchanged baseline and candidate executables back-to-back under the
same local CPU load, including `LATENCY_CALLBACK_JITTER=1`. The harness uses
virtual callback threads, so it measures pipeline scheduling rather than CPAL
callback priority. `receiver_playout.underruns` and `headroom_samples` are the
active-playback counters; lower latency alone does not establish better scheduling.

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
  --target-dir /path/to/baseline/target/latency-build --release -p gwar-client --example latency
cargo build --manifest-path /path/to/candidate/Cargo.toml \
  --target-dir /path/to/candidate/target/latency-build --release -p gwar-client --example latency
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

Prepare an SSH-accessible checkout with a built `gwar-server` and open the chosen
UDP port. The runner builds and installs nothing remotely. Choose unused ports
and supply all addresses and paths explicitly:

```sh
python3 scripts/latency/run_remote.py \
  --ssh-host user@your-host \
  --public-ip YOUR_PUBLIC_IP --media-bind YOUR_REMOTE_INTERFACE_IP \
  --media-port 9990 --http-port 8791 --tunnel-port 18791 \
  --remote-checkout /absolute/remote/checkout \
  --server-binary target/release/gwar-server \
  --variant current=target/release/examples/latency \
  --markers 100 --runs 4 --out target/latency/remote
```

IPv4/IPv6 are accepted; the media bind IP may differ from the advertised public
IP behind NAT. Remote binary paths can be absolute or checkout-relative. To
interleave complete client/server builds, replace `--server-binary` with repeated
`--server-variant baseline=target/baseline/release/gwar-server` and
`--server-variant candidate=target/candidate/release/gwar-server`, supplying matching
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
of safety. Arrival phase contributes no credit. Each correction proposes at most
720 samples (15 ms). Each committed cut is subtracted from every historical
margin, so later corrections cannot spend the same surplus twice. Depth is
buffered time from the playout position to the end of the queue, holes
included, and every observation is revised in hindsight by the arrivals after
it: the first packet at or beyond the queue's end extends it over the missing
packets (they were lost and are concealed, never late), while a packet arriving
inside the span pulls it back to itself (it was not there). Under the TS sequence
clock a key stride says nothing about duration, so an observation keeps the
durations of the packets queued then and counts a hole as the shortest of them. Only the present
packets used to count, so every loss looked like a one-packet dip at the tail;
with a loss every couple of seconds the window never cleared and a 10--20 ms
residual stayed for good (the receiver's "level steps"). Concealment samples
the queue with the refill demand the PLC left, not the whole demand again. An
empty queue still vetoes shrinking for the full window unless a later arrival
proves a loss; a dip whose fate is still unknown vetoes a cut, but only a
settled low mark restarts the shrink hold. Corrections are at least 500 ms apart (at most 10 ms/s
for quiet audio or 30 ms/s for a correlated 15 ms period). Surplus below 120
samples is not corrected. Startup, observation gaps and stall recovery still
require a fresh window. The post-trim concealment counter retains its one-second
window independently of the correction interval.

Voiced corrections search 120--720 sample lags within the available budget,
using mean-centered normalized correlation (>0.98) and similar window energies.
One selected lag is removed with a linear overlap-add lasting that entire lag;
`shrunk` receives exactly the number removed. Speech onsets remain protected,
including onsets in lookahead packets. Correlated speech need not wait for the
old nonquiet deadline; uncorrelated audio never uses that deadline to force a
cut. Quiet audio retains the <=240-sample skip and 120-sample boundary fade.
Burst recovery still discards whole packets and fades as before.

A 10 ms packet cannot contain two periods of low-pitched speech. Only when a
correction is eligible, the decoder stages already queued contiguous packets
until it can search the full budget (at most 30 ms of search input). It never
waits for another packet or conceals a missing lookahead packet, and stops before
stall recovery. This adds no fixed startup delay or played-sample history; the
extra decoded tail remains included in depth accounting. A pitch period larger
than the safe budget is left as residual latency until quiet audio arrives.
Synthetic tests compare splice jumps and magnitude-spectrum error against an
equal-count skip with the previous 120-sample fade, using 100--300 Hz harmonic
signals with and without 2% / 5 Hz vibrato. These are waveform checks, not a
listening test or a guarantee of perceptual transparency on real speech.

Local paired check (2026-10-14), `bdaac83` baseline then TSM candidate for each
scenario, 100 markers per run; impairment specs use `seed=1` exactly once.
Values below are baseline / candidate; trim totals include burst recovery.
Raw CSV/JSON, logs, binary/source hashes and run order are retained locally in
`target/latency/tsm-pairs/`; the local runner is `target/tsm_pairs.py`.

| Scenario | Median ms | p95 ms | Concealments | Trimmed ms | Trim events | Concealments after trim |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| No impairment | 56.68 / 47.19 | 59.93 / 50.72 | 2 / 2 | 5 / 0 | 1 / 0 | 0 / 0 |
| up:stall=270ms@12s | 50.09 / 47.54 | 60.47 / 50.35 | 13 / 11 | 260 / 250 | 1 / 1 | 0 / 0 |
| down:jitter=10ms | 57.33 / 57.21 | 60.50 / 60.74 | 3 / 2 | 0 / 0 | 0 / 0 | 0 / 0 |
| down:loss=2% | 47.82 / 46.81 | 57.17 / 50.24 | 64 / 64 | 0 / 0 | 0 / 0 | 0 / 0 |

All runs passed the matching gate: 99/100 markers for both stall runs and 100/100
otherwise, with zero active-stream underruns. No candidate run performed a
fractional correction (its sole trim was whole-packet stall recovery), so this
comparison demonstrates no latency improvement attributable to TSM. These are
single pairs on a shared Mac, not controlled idle-host measurements. Synthetic
splice tests did show 39.5--99.98% less normalized magnitude-spectrum error than
the equal-count skip/fade baseline across all 36 tone/vibrato/phase cases.

Level steps (2026-10-20). Under the mixed profile (`up:delay=20ms,jitter=5ms,
keep_order,stall=270ms@12s,every=10s;down:loss=1%,burst=3@0.5%,reorder=0.5%`)
marker latency moved between 67, 77 and 95 ms from one stall to the next. The
harness now writes `{output}.timeline.csv` (playout state every 100 ms); it shows
the queue holding one extra 10 ms packet (mean 25 instead of 16 ms) in the 77 ms
windows. Two causes: stall recovery kept the packet straddling its deadline (the
stall rarely ends on a packet boundary; the sequence clock also rounded the
deadline down), and the fractional drain needs a fresh five-second window after
each stall, so one packet of excess usually survives until the next. Recovery now
discards every packet that starts before the deadline, still keeping the target
and the pending refill. Separately, loss no longer counts as a low-water dip (see
above); that matters for steady streams with frequent loss and is neutral here.

Local runs, 2026-10-20, 200 markers, median per 10 s window between stalls
(windows over 72 ms count as a step). Single runs on a shared Mac; harness exit
status 1 because the mixed profile loses more than 5% of markers by design.

| Build | Seeds | Stepped windows | Mean of window medians | Concealments / run | Late drops / run |
| --- | --- | ---: | ---: | ---: | ---: |
| `cb004d8` | 3, 5, 7 | 4 / 15 | 71.1 ms | 216 | 21.7 |
| hindsight marks only | 3, 5, 7, 11, 13, 17 | 11 / 30 | 72.7 ms | 222 | 18.2 |
| + recovery rounding | 3, 5, 7, 11, 13, 17 | 5 / 30 | 69.5 ms | 226 | 21.5 |

The remaining steps follow longer loss bursts or stalls while the jitter
estimate is high. The drain still subtracts the jitter reserve from a low-water
minimum that already contains the jitter dips, and restarts its window after
every stall; relaxing either would drain faster at some risk of late packets and
is left as a separate decision.

Receiver recovery follow-up (2026-10-16; measurement pending). Low-water
observations now survive isolated PLC and are debited after each cut, retaining
the five-second jitter protection without repeatedly restarting it. Deterministic
10/20 ms tests in RTP and TS sequence clocks recover a 100 ms burst backlog
within 20 seconds despite one loss per second, to at most the target plus 5 ms
(safety and minimum correction quantum), with no extra concealments. Sparse
5 ms jitter every three seconds still receives no cuts. This quiet-audio bound
is not a bound for uncorrelated speech: the decoder can still decline a cut.
Speaker headroom keeps its 5 ms/event attack and 40 ms cap but releases 5 ms per
release interval, including intervals with underruns, instead of requiring 30
seconds of complete silence per step. The interval starts at 5 s and doubles (up
to 80 s) whenever an underrun follows a release within 5 s, so a periodic stall
that needs the margin costs ever rarer glitches; a release without an underrun,
or an underrun no release caused (headroom did not prevent it), resets it to
5 s. Rare misses therefore cannot pin 40 ms.

Native PCM and encoded queues retain callback capture time and discard audio
older than 50 ms. PCM age is residence in the ring, so device callbacks longer
than 50 ms still complete their frames; a frame's age starts with its last
sample. End-of-speech markers are never age-dropped and are queued rather than
dropped under socket pressure (direct TS and puppets), so a late marker still
ends the TS talk spurt. A puppet whose full uplink dropped the marker ends the
spurt itself once the stale backlog is drained.
Direct TS carries this time through its second queue and uses immediate UDP
writes. Puppet uplinks retain local transport-admission time across fanout and
the 40 ms packetizer tail; queued packets, held tails and final writes expire
after 50 ms. This bounds local residence, not unknown remote capture/network
age. Puppet socket-pressure dropping was already immediate and is unchanged.
Diagnostics add native `capture_stale_samples`, `capture_stale_frames`,
`capture_overflow_samples`, `outgoing_full_dropped`, direct-TS handle
`audio_stale_dropped` / `audio_socket_pressure`, and puppet `voice_stale_dropped`.
Quiet-machine latency and real TeamSpeak measurements remain pending; the
latency harness was not run for this follow-up.

Local loss investigation (2026-10-16). The original harness reproduced
`seed=42;down:loss=2%` at 777/782 ms median/p95 on `21ee239`, versus 47/50 ms
with seed 1. The older `bdaac83` endpoint also measured 777/783 ms with seed 42,
so the failure already existed at the October 14 baseline. The earlier runner
used the same detector, callbacks, 100 markers and environment clearing; its
seed-1 loss setting explains the healthy result.

The relay impairs handshake packets as well as RTP. A diagnostic seed-42 trace
dropped downlink flow 0 datagram 3, a DTLS flight, and the native sender wrote
80 captured frames into str0m before its delayed `Connected` event. Burst loss
also dropped the next STUN reply and DTLS retry; that trace queued 297 frames.
The receiver then got about a second of audio in a few milliseconds before its
first playout. Its 64-packet cap retained old audio and dropped fresh arrivals;
recurring PLC kept restarting the five-second surplus observation window.
The target remained 10 ms while the queue stuck near 640 ms.

`f58a0f7` drops native capture while ICE/DTLS/SRTP is unavailable and discards a
bounded snapshot of pending capture on connection, preserving capture-clock
timestamps. A real-transport regression sends 100 old frames before allowing
the handshake: the first received payload must be the fresh frame at timestamp
48000. It fails before the fix and when the production fix is reverted, then
passes when restored. A delayed SFU-listener control already passes unchanged.
The fix changes no jitter/PLC/trim policy or latency constants.

Connection retries can exceed the original two-second marker warmup. An
original-harness fixed burst run had 37/40 ms median/p95 and a 10 ms final queue,
but only 81/100 matches, including 17 missing leading markers. The paired
playout measurements use `LATENCY_WAIT_CONNECTED=1` for both variants: carrier
and impairments continue through setup, then markers start after connection
and the normal two-second warmup. Existing queued audio is retained, so the
baseline failure remains observable. Connection time is recorded separately;
these matching results measure connected playout, not connection availability.

Paired check: production baseline `21ee239` and candidate `f58a0f7`, both
with the identical optional harness change, 100 markers × four runs per
variant in ABBAABBA order for each profile. Apple M5, macOS 27.0, Rust 1.99.0,
Opus 1.6.1; sequential loopback runs on a shared interactive host, with no
concurrent task builds/tests. Seed 42 is used except the explicit seed-1 row
and the documented unseeded upstream stall. Values are baseline / candidate.
Quantiles pool all matched markers, including failed runs; PLC and underruns
sum four runs, and final packet-queue sizes show the four-run range.

| Profile | Median ms | p95 ms | p99 ms | Matched /400 | PLC | Underruns | Final queue ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Idle | 42.46 / 41.80 | 47.82 / 47.44 | 49.77 / 48.64 | 400 / 400 | 0 / 0 | 0 / 0 | 0 / 0–10 |
| Loss 2%, seed 42 | 777.19 / 37.87 | 781.87 / 118.03 | 784.72 / 118.97 | 381 / 400 | 1695 / 250 | 0 / 3 | 620–640 / 0–60 |
| Loss 2%, seed 1 | 37.60 / 37.49 | 67.29 / 57.65 | 68.32 / 67.22 | 400 / 400 | 256 / 253 | 1 / 1 | 10–30 / 10–20 |
| Burst 3@0.5% | 866.58 / 38.72 | 872.29 / 48.26 | 872.98 / 49.22 | 344 / 396 | 2757 / 193 | 0 / 0 | 630–640 / 10 |
| Reorder 0.5% | 37.33 / 37.33 | 46.73 / 39.26 | 48.96 / 47.34 | 400 / 400 | 64 / 66 | 0 / 0 | 0–20 / 10–20 |
| Jitter 10 ms | 47.40 / 47.32 | 59.17 / 48.91 | 67.98 / 49.45 | 400 / 400 | 8 / 4 | 0 / 0 | 20–30 / 10–20 |
| Upstream stall 270 ms | 42.33 / 38.17 | 47.73 / 47.74 | 48.89 / 49.29 | 396 / 396 | 40 / 43 | 0 / 0 | 0–10 / 10–20 |
| Mixed (documented spec) | 68.69 / 67.44 | 797.23 / 77.39 | 799.31 / 78.73 | 371 / 386 | 759 / 410 | 0 / 0 | 10–20 / 10 |

All 32 candidate runs passed the ≥95% matching gate (3178/3200 markers);
24/32 baseline runs passed (3092/3200). The runner's fail-fast errors were kept,
then each unattempted slot in the original order was run once and validated
with the same gate. Nothing was retried or excluded. One seed-42 candidate
run measured 117 ms median with three output underruns, 25 ms speaker
headroom, 60 ms queued packets and 5 ms staged PCM. It contributes the 118 ms
pooled tail. Whether that residual reflects shared-host scheduling alone or
also warrants a separate recovery-policy change remains open; the fix does
not establish an unconditional tens-of-ms bound after scheduling misses.
Other final staged tails were 0–7.5 ms, and speaker queues were 20 ms except
underrun cases (25–35 ms). These are virtual-device results.

Raw CSV/JSON, logs, exact per-profile commands, wrapper environment settings,
binary/source hashes, original-harness reproductions, handshake traces and the
common baseline harness patch are retained in
`/private/tmp/gwar-playout-loss-latency/`; `matrix.py` invokes `run_local.py`
with `--markers 100 --runs 4`, and `analysis.json` retains every run's counters
and final queue/staged/speaker sizes. The two temporary historical/baseline
worktrees and their targets were removed. Formatting and workspace Clippy
with `-D warnings` passed; workspace tests: 641 passed, 0 failed, 7 ignored
(TeamSpeak fixtures disabled); latency-example tests: 16 passed. The native
regression passed after restoration and failed on both the original code and
the explicit revert-fix mutation.

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

## Native 10 ms packets in Chromium

The manual `apps/web/e2e/native-audio.mjs` check starts a fresh loopback-only
server, the real native client with virtual audio devices, and the built web app
in headless Chromium. It measures two 10-second steady-state windows after two
seconds of warmup each. Native tones are 1000 Hz and 1500 Hz; Chromium sends a
looped 700 Hz fake-microphone WAV with capture processing disabled. Decoded tones
must reach the app's WebAudio master output and the native speaker ring.
The test visits the other seven fixed receive slots before reusing the first
with a new native identity, checking that its track, SSRC and peer connection
stay the same. Stopping native audio must produce silence in the browser.

```sh
GWAR_CHECKOUT=/absolute/path/to/gwar
pnpm --dir "$GWAR_CHECKOUT" install --frozen-lockfile
pnpm --dir "$GWAR_CHECKOUT" --filter web exec playwright install chromium
cargo build --manifest-path "$GWAR_CHECKOUT/Cargo.toml" \
  --target-dir "$GWAR_CHECKOUT/target" -p gwar-client --example web_audio
pnpm --dir "$GWAR_CHECKOUT" --filter web e2e:native-audio
# Optional diagnostic run through an additional loopback UDP hop:
E2E_RTP_TRACE=1 pnpm --dir "$GWAR_CHECKOUT" --filter web e2e:native-audio
```

Runs take about 31 seconds after compilation. Each retains `report.json`, SDP,
process/build logs and the WAV under `target/web-audio/<run>/`. No existing
server, physical audio device, Connect service or TeamSpeak server is used.
The script creates its own endpoints and rejects non-loopback browser requests.
Only processes it starts are stopped. The optional trace records receive times and
clear RTP sequence/timestamp/SSRC/marker headers, forwards to `Running.media_bind`,
and reports unused ICE-candidate control-reply errors separately; RTP delivery
errors fail.

Stats are matched to the speaker's `voice.slot` and receiver MID. Counters use
window differences: concealment is `delta(concealedSamples) /
delta(totalSamplesReceived)`; average buffering is `delta(jitterBufferDelay) /
delta(jitterBufferEmittedCount)` ([WebRTC stats definitions](https://www.w3.org/TR/webrtc-stats/)).
The check requires about 100 packets/s, about 480 decoded samples/packet, zero
loopback packet loss, less than 1% concealment and less than 60 ms mean buffering.
Raw counters, target/minimum buffering, per-second snapshots and both PCM tone
checks are retained even when the final quality assertions fail.

For longer controls, set `E2E_AUDIO_RESEARCH=1` (removes only the 60 ms ceiling),
`E2E_AUDIO_PRE_SECONDS=30`, `E2E_AUDIO_POST_SECONDS=60` and
`E2E_AUDIO_SCENARIO=reuse|pause|prewarmed|continuous`. `pause` mutes/unmutes the
same running native engine after `E2E_AUDIO_GAP_MS` (default 2200); `prewarmed`
activates an already-connected, muted native engine after one second.
`E2E_AUDIO_SENDER=browser` compares the browser's 20 ms sender (except prewarmed).
The fixture keeps native signaling alive during long windows. Summarize reports
with `python3 apps/web/e2e/native-audio-summary.py target/web-audio/<run>`.

Chromium 153 on macOS reported `audioLevel=0` and `totalAudioEnergy=0` while the
actual app output carried the native tone at about 0.176 RMS. The app uses a muted
media element to feed WebAudio; libwebrtc delivers PCM to that external sink
before applying output gain, then measures level/energy after that gain
([receive implementation](https://webrtc.googlesource.com/src/+/refs/heads/main/audio/channel_receive.cc)).
These stats are recorded, but the PCM tone check determines playback.

Round 2 ran 13 completed controls (raw counters kept locally, not committed)
with 30–60-second post-switch windows. Prewarming removed the initial burst, yet
buffering rose 37.83→48.58 ms and remained 49.14 ms in the final ten seconds;
target/minimum stayed at 40 ms after reuse. Another prewarmed run retained about
29.3 ms against an unchanged 20 ms target. Same-engine mute/resume averaged
27.43→27.42 ms. The 20 ms browser control also rose initially (32.85→35.27 ms),
then reached 19.92 ms. A target increase is not necessary for the rise.

Switch clock offsets stayed below one packet. Experimental single-sample
anchoring reduced them to −0.20/+0.17 ms, but buffering changed +4.07/−2.05 ms
versus +0.80/+3.41 ms in interleaved rounded runs; no clock fix is justified.
The tested [NetEQ logic](https://webrtc.googlesource.com/src/+/9ea5afcad008b940468c2a15aec339592cf5a935/modules/audio_coding/neteq/decision_logic.cc)
postpones decoding after prolonged concealment and allows 20 ms plus arrival
variation above target before acceleration. Retained delay history, recovery
and this hysteresis best explain the results; internal decisions were not traced.
All completed measurement phases had zero packet loss and under 0.14% concealment. Shared-host
scheduling, physical devices, other browsers and internet paths remain unisolated;
these measurements do not establish a stable CI performance limit.

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
a sequence restart requires a fresh SSRC or voice session. At most 32 retired
SSRCs are retained without eviction; exhaustion closes voice with `stream_limit`.
TeamSpeak uses its wrapping u16 packet id times 960 as a synthetic clock,
with half-range ordering and separate voice/whisper identities; every listener
sees the same ids (checked against the official server in CI). The ids do not
advance across silence, so the bridge numbers each talk spurt (per speaker, and
per speaker and target for whispers) and a new spurt re-anchors the slot on wall
time, even for a listener that missed the end marker. An out-of-order id more
than a second after the last write starts a new clock instead of being dropped. Both bridge directions retain Opus durations.

First packets start at 48000 ticks. Speaker/run/SSRC changes, source-clock
generation changes and deltas over five seconds advance by elapsed wall time
rounded up to 480 ticks (at least 480), with the previous forwarded packet's
span as a floor. Plaintext and TeamSpeak use parsed duration; opaque encrypted
RTP uses the source's last positive in-order step clamped to 960–5760 ticks,
initially 960. Source resets are recorded before mute/speech gating, so omitted
reset packets cannot hide a discontinuity from a slot. The slot clock remains
u64 internally and emits low u32 ticks. Speaker reassignment also increments
the existing slot generation. See [the 5b report](voice-sfu-5b.md). Zero deltas
drop: forwarding the same media instant would duplicate playout. Dropping reordered packets keeps each output clock monotonic and lets
the receiver conceal loss without replaying stale audio. Very long genuine
pauses use the same wall-time fallback as a clock reset.

Local release comparison against `26bc30d` (three 100-marker runs per build):
baseline/candidate median 40.78/41.48 ms, p95 47.47/47.55 ms, p99 49.68/49.55 ms;
298/300 and 299/300 markers matched, respectively, with no receiver underflows.
No builds, tests or injected load ran during measurement; Spotlight remained
active, so these were not fully idle-host measurements. Real internet impairment,
live browser playback and official TeamSpeak interoperability remain unverified.

## Relative-delay estimator experiment (2026-10-14)

A NetEQ-style replacement for the peak-hold estimator was tried and rejected
(branch `experiment/jitter-quantile`, not merged): relative arrival delay against the fastest
transit in a two-second window, a 97th-percentile histogram of 200 ms maxima
with 20 s forgetting, and a three-packet fast attack. In two harness pairs per
case it helped downlink jitter (`down:jitter=10ms` p95 97.9 → 59.7 ms) but
regressed the unimpaired relay (median 46.0 → 58.0 ms) and the documented mixed
spec (concealments 246 → 411). The target only limits shrinking, and the
five-second low-water rule already retains periodic spikes, so longer estimator
memory mostly kept extra delay. Other relative-delay designs remain untested.
