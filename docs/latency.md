# Native mouth-to-ear latency

`vc-client`'s `latency` example feeds synthetic audio into one native client's
microphone ring and timestamps decoded marker onsets at another's speaker ring.
It includes capture batching, Opus, WebRTC, the Gwar SFU, jitter buffering,
decoding and virtual speaker playout. Both clients run on the measuring host and
share a monotonic clock, including with a remote server. Physical audio devices,
browsers, TeamSpeak and Gwar Connect are not used.

Virtual devices run 48 kHz mono with phased 480-sample (10 ms) callbacks. After
two seconds of warmup, a quiet 400 Hz carrier contains a 20 ms, 1800 Hz marker
every 250 ms. A trailing 5 ms RMS window detects onset (on/off thresholds
0.18/0.11). Each capture matches the first onset within the following 250 ms;
unmatched markers are lost. Detector delay and callback batching are included
without correction. This approximates native mouth-to-ear latency; acoustic
latency and physical device timing need a separate measurement.

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
removed on completion, errors or Ctrl-C. Allow roughly `3 + 0.25 * count` seconds
plus setup. Unset `LATENCY_URL` and `LATENCY_SERVER` for local mode; the local
runner clears inherited `LATENCY_*` and `VC_*` settings itself.

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

The CSV contains matched marker index, capture/playout seconds, latency in ms
and playout sample index. The adjacent `.csv.json` keeps requested `markers`,
matched `n`, lost marker indices, onset count, median/p95/p99, receiver underflow
samples/frame counters, voice events, addresses and detector parameters.
Quantiles use empirical nearest rank, including the lower median for even n;
all-lost quantiles are `null`. CSV/JSON are written before rejecting more than
5% lost markers; losses never shift subsequent matches.

Runners require a new `--out` directory and accept only ≥95% matched markers with
consistent CSV/JSON counts. They never retry or exclude runs automatically.
Output includes per-run CSV/JSON/client logs, `manifest.json` (order, binary
SHA-256 hashes and metadata), combined `latency.csv`, and `summary.json` (pooled
quantiles and deltas from the first variant). Failed series record an error in
the manifest. Remote runs also retain server and tunnel logs.

Compare loss and underflows alongside latency. Frame counters cover warmup too.
Inspect individual call levels: startup buffering or network drift can dominate
pooled p95. The runners impose no fixed performance pass threshold.
