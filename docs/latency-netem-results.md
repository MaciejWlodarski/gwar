# Loopback UDP impairment relay: release measurements

Measured on 2026-10-13 in `gwar-p3-netem`, branch `p3/netem-harness`,
against engine/base commit `a0f568603d1d32960c01574ebe5009c76bc88623`. The voice engine is unchanged.
Every corrected run used 100 markers and the same release example binary.
Binary SHA-256: `32788c6d8e062f1beb7e946d634395719aa209323628dbc529653066fe808b6b`.

The example reserves a loopback relay port and supplies it through the existing
`Config.media_advertise`. Each client has its own connected upstream UDP socket;
server replies return from the advertised relay port. Signaling remains direct.
The only production server change is the additive `Running.media_bind` field
exposing the actual bound UDP address. `Running.media` keeps its advertised-address
meaning. There is no new server flag or change to production defaults.

The scheduler supports per-direction delay, signed bounded uniform/normal jitter,
order preservation, one-shot/periodic stalls released as bursts, independent loss,
consecutive burst loss, probabilistic reorder holds, and fixed SplitMix64 seeds.
For syntax and accounting, see [latency.md](latency.md#loopback-network-impairments).
Example: `LATENCY_NET="seed=42;up:stall=270ms@12s,jitter=5ms;down:loss=1%,reorder=0.5%"`.

All requested checks passed:

- `cargo fmt --all -- --check`.
- `cargo clippy --workspace --exclude vc-desktop --all-targets -- -D warnings`.
- `cargo test --workspace --exclude vc-desktop`: 359 passed, 5 existing ignored tests.
- `cargo test -p vc-client --example latency`: 16 passed (9 new relay/scheduler tests).
- `cargo test -p vc-server --test media`: 2 passed, including advertised/bound port separation and unchanged defaults.
- `cargo build --release -p vc-client --example latency`.

The scheduler tests cover deterministic seeds, both jitter distributions and bounds,
packet deadlines, order preservation, actual reorder accounting, stall boundaries
and periodic bursts, in-flight packets, burst loss, full loss, strict parsing,
and isolation of two clients and their return addresses.

| Run | LATENCY_NET | Matched | Median ms | p95 ms | p99 ms |
| --- | --- | ---: | ---: | ---: | ---: |
| direct | unset (direct) | 100/100 | 56.855 | 68.355 | 70.778 |
| relay | `seed=1` | 100/100 | 49.621 | 109.228 | 109.983 |
| up-jitter | `up:jitter=10ms` | 100/100 | 103.130 | 138.667 | 139.391 |
| down-jitter | `down:jitter=10ms` | 100/100 | 62.934 | 69.936 | 70.744 |
| both-jitter | `up:jitter=10ms;down:jitter=10ms` | 100/100 | 81.221 | 88.751 | 89.422 |
| up-stall | `up:stall=270ms@12s` | 100/100 | 286.996 | 297.565 | 298.889 |
| down-stall | `down:stall=270ms@12s` | 99/100 | 49.835 | 58.282 | 59.732 |

Quantiles use the harness's empirical nearest rank (lower median for even n).
Only down-stall lost a marker: marker 40. All corrected runs passed the existing
matching floor/window, voice-event, and ≥95% match gates. The no-impairment relay
had zero scheduled delays, drops, reorderings or pending packets in both directions.

These are individual call measurements, not controlled comparative estimates.
The host's one-minute load average ranged from 5.95 to 42.10 during the series.
The direct and relay runs contain spontaneous buffering changes; jitter results
include those host/engine effects as well as the specified network delay. Seeds
reproduce decisions for the same packet arrival trace, not OS callback timing.

An initial attempt is retained under `target/netem-results/initial/`: its direct
run measured 182.419 ms median; relay pass-through failed because the harness
initially used `Running.media` as the upstream address, causing a relay loop.
The next jitter attempt was interrupted. These are separate from the corrected
series above; no failed attempt was silently substituted into that series.

**Uplink stall:** persistent added latency reproduced. Marker 39 at 11.952954 s
measured 48.314 ms; marker 40 at 12.202674 s jumped to 298.504 ms (+250.190 ms).
The pre-stall median (capture <11.8 s) was 47.391 ms;
the last-ten-marker median was 286.996 ms.
Marker 99 at 26.952100 s still measured 287.037 ms. The 270 ms configured stall
therefore left roughly 240–250 ms of added measured latency through the last marker,
with a small decline over time; it did not return to baseline in this call.
The receiver finished with 12 queued packets (11,520 samples), 720 shrunk samples
(15 ms), and 3 trim events. These observations describe this engine; no fix was made.

**Downlink stall:** recovered, with marker 40 unmatched. Marker 39 measured
49.485 ms and marker 41 at 12.451614 s measured 57.153 ms, so the first matched
marker after release was already close to baseline; there was no persistent
270 ms offset. Marker 99 measured 49.012 ms.
The pre-stall median was 46.501 ms;
the last-ten-marker median was 49.227 ms.
The receiver finished with 1 queued packet, 11,880 shrunk samples (247.5 ms),
and 3 trim events. No latency can be assigned to the unmatched marker.

Applied stalls are on the measurement clock, in seconds. Windows are reported
separately for each client flow (all UDP traffic, including control packets):

| Run/direction | Flow | Planned start–end s | Held packets | First arrival s | First–last actual release s |
| --- | ---: | --- | ---: | ---: | --- |
| up-stall/up | 0 | 12.000–12.270 | 13 | 12.013265583 | 12.272555292–12.272608708 |
| up-stall/up | 1 | 12.000–12.270 | 1 | 12.046085917 | 12.272614958–12.272614958 |
| down-stall/down | 0 | 12.000–12.270 | 5 | 12.052887292 | 12.271686750–12.271723250 |
| down-stall/down | 1 | 12.000–12.270 | 13 | 12.015311042 | 12.271729834–12.271809667 |

All stall-run packet counters reconcile (`received = forwarded + dropped + pending`).
Neither relay deliberately dropped or reordered packets in these stall runs; pending
counts are zero. The raw CSV/JSON/logs and the ordered manifest remain in
`target/netem-results/`; the manifest records specs, exits, binary hash and host load.

The complete per-marker stall timelines follow (zero-based indices, raw capture
times rounded to 6 decimals and latency rounded to 3 decimals).

| Marker | Uplink capture s | Uplink latency ms | Downlink capture s | Downlink latency ms |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 2.203227 | 47.439 | 2.203352 | 46.501 |
| 1 | 2.453564 | 47.491 | 2.450165 | 49.635 |
| 2 | 2.703398 | 46.496 | 2.704827 | 42.870 |
| 3 | 2.952230 | 49.636 | 2.952857 | 47.032 |
| 4 | 3.203053 | 49.160 | 3.203889 | 46.066 |
| 5 | 3.453390 | 45.746 | 3.452332 | 47.984 |
| 6 | 3.702442 | 45.116 | 3.704051 | 45.754 |
| 7 | 3.952240 | 47.391 | 3.953485 | 46.104 |
| 8 | 4.203675 | 47.357 | 4.203238 | 45.445 |
| 9 | 4.452014 | 48.605 | 4.453206 | 47.381 |
| 10 | 4.702330 | 47.074 | 4.703763 | 43.710 |
| 11 | 4.953758 | 47.351 | 4.951535 | 49.049 |
| 12 | 5.203464 | 45.893 | 5.200530 | 49.615 |
| 13 | 5.451135 | 48.866 | 5.454196 | 44.433 |
| 14 | 5.703564 | 46.227 | 5.702035 | 47.735 |
| 15 | 5.950511 | 49.597 | 5.954701 | 45.141 |
| 16 | 6.203350 | 47.250 | 6.203346 | 46.484 |
| 17 | 6.452100 | 47.828 | 6.453375 | 46.266 |
| 18 | 6.702219 | 47.646 | 6.701657 | 48.950 |
| 19 | 6.952376 | 48.215 | 6.950212 | 50.443 |
| 20 | 7.202831 | 47.830 | 7.203335 | 46.911 |
| 21 | 7.451866 | 48.859 | 7.453927 | 46.132 |
| 22 | 7.700760 | 49.726 | 7.703356 | 46.222 |
| 23 | 7.951396 | 48.727 | 7.950307 | 50.255 |
| 24 | 8.202200 | 48.322 | 8.203935 | 46.113 |
| 25 | 8.452195 | 45.947 | 8.452076 | 45.437 |
| 26 | 8.703345 | 47.329 | 8.702744 | 47.114 |
| 27 | 8.953483 | 45.626 | 8.951305 | 50.159 |
| 28 | 9.202189 | 45.432 | 9.203270 | 47.538 |
| 29 | 9.453352 | 46.316 | 9.454295 | 44.593 |
| 30 | 9.704701 | 46.698 | 9.702566 | 46.680 |
| 31 | 9.950829 | 48.057 | 9.951349 | 46.706 |
| 32 | 10.201201 | 49.769 | 10.203344 | 45.704 |
| 33 | 10.454163 | 45.857 | 10.451949 | 48.986 |
| 34 | 10.702186 | 46.873 | 10.702512 | 46.895 |
| 35 | 10.952186 | 46.917 | 10.953252 | 44.685 |
| 36 | 11.203097 | 46.194 | 11.203497 | 45.234 |
| 37 | 11.452811 | 47.474 | 11.452428 | 48.273 |
| 38 | 11.703177 | 47.757 | 11.702909 | 45.283 |
| 39 | 11.952954 | 48.314 | 11.950143 | 49.485 |
| 40 | 12.202674 | 298.504 | 12.203785 | unmatched |
| 41 | 12.453322 | 297.207 | 12.451614 | 57.153 |
| 42 | 12.702700 | 296.036 | 12.702872 | 57.379 |
| 43 | 12.953733 | 296.052 | 12.951731 | 58.632 |
| 44 | 13.203540 | 297.586 | 13.204220 | 57.667 |
| 45 | 13.453392 | 297.350 | 13.452009 | 58.699 |
| 46 | 13.703965 | 295.777 | 13.703815 | 56.206 |
| 47 | 13.952176 | 297.452 | 13.954749 | 57.182 |
| 48 | 14.203558 | 296.905 | 14.201262 | 57.550 |
| 49 | 14.454737 | 296.528 | 14.451713 | 59.732 |
| 50 | 14.702810 | 294.921 | 14.703769 | 58.093 |
| 51 | 14.952148 | 296.231 | 14.950663 | 57.253 |
| 52 | 15.202685 | 298.889 | 15.204514 | 53.279 |
| 53 | 15.450229 | 300.620 | 15.450250 | 58.443 |
| 54 | 15.702152 | 295.974 | 15.703787 | 56.813 |
| 55 | 15.953154 | 296.864 | 15.953187 | 57.023 |
| 56 | 16.204174 | 297.565 | 16.200297 | 57.575 |
| 57 | 16.452521 | 295.427 | 16.452317 | 58.282 |
| 58 | 16.703383 | 294.762 | 16.703904 | 56.761 |
| 59 | 16.953430 | 298.678 | 16.953520 | 56.448 |
| 60 | 17.200645 | 296.379 | 17.203409 | 57.157 |
| 61 | 17.453520 | 292.154 | 17.453794 | 52.379 |
| 62 | 17.702925 | 292.055 | 17.703153 | 52.629 |
| 63 | 17.953242 | 291.904 | 17.950489 | 54.828 |
| 64 | 18.203137 | 291.911 | 18.202528 | 53.935 |
| 65 | 18.452022 | 293.541 | 18.453802 | 51.504 |
| 66 | 18.703883 | 291.337 | 18.700414 | 52.132 |
| 67 | 18.951896 | 293.544 | 18.953339 | 52.811 |
| 68 | 19.200164 | 295.337 | 19.202074 | 53.448 |
| 69 | 19.453023 | 292.351 | 19.450248 | 53.323 |
| 70 | 19.700059 | 295.416 | 19.703576 | 53.636 |
| 71 | 19.950309 | 295.060 | 19.951972 | 50.940 |
| 72 | 20.203332 | 292.113 | 20.204817 | 47.990 |
| 73 | 20.453440 | 291.539 | 20.453264 | 52.347 |
| 74 | 20.703804 | 291.918 | 20.703353 | 53.472 |
| 75 | 20.953211 | 291.024 | 20.953696 | 52.474 |
| 76 | 21.200513 | 292.830 | 21.201764 | 54.761 |
| 77 | 21.450897 | 293.264 | 21.451355 | 54.375 |
| 78 | 21.703773 | 290.370 | 21.703620 | 50.886 |
| 79 | 21.951200 | 292.213 | 21.950991 | 54.842 |
| 80 | 22.201905 | 287.256 | 22.203631 | 51.202 |
| 81 | 22.454884 | 283.538 | 22.451290 | 51.605 |
| 82 | 22.704164 | 287.960 | 22.700257 | 52.024 |
| 83 | 22.953102 | 285.115 | 22.950867 | 52.317 |
| 84 | 23.203268 | 284.228 | 23.202717 | 50.225 |
| 85 | 23.450680 | 287.887 | 23.454070 | 49.645 |
| 86 | 23.703422 | 286.593 | 23.703428 | 48.725 |
| 87 | 23.954456 | 284.508 | 23.951537 | 49.489 |
| 88 | 24.201812 | 289.190 | 24.203344 | 49.990 |
| 89 | 24.452092 | 289.371 | 24.453258 | 49.828 |
| 90 | 24.703192 | 287.927 | 24.703241 | 48.093 |
| 91 | 24.953139 | 286.996 | 24.952386 | 49.835 |
| 92 | 25.203383 | 287.081 | 25.203826 | 49.227 |
| 93 | 25.450857 | 289.015 | 25.452718 | 47.292 |
| 94 | 25.700617 | 286.879 | 25.703824 | 46.132 |
| 95 | 25.953100 | 284.677 | 25.953825 | 49.476 |
| 96 | 26.201181 | 287.960 | 26.203867 | 49.895 |
| 97 | 26.453532 | 285.766 | 26.453826 | 49.289 |
| 98 | 26.702635 | 285.148 | 26.703407 | 50.221 |
| 99 | 26.952100 | 287.037 | 26.953278 | 49.012 |
