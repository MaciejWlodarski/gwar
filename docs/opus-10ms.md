# Native 10 ms Opus

Native WebRTC capture uses `voice::FRAME = 480` at 48 kHz. This is the single
switch back to 960/20 ms; direct TeamSpeak uses a separate fixed 960-sample
capture quantum. Both paths retain `Application::Voip`, 48 kbps, in-band FEC,
a 5% packet-loss hint and automatic signal/bandwidth selection. RTP timestamps
use the native capture quantum, retain wall-clock pauses, and advance at least
one quantum when a device or transport wakeup batches several packets.

## Compatibility and receive policy

- Direct TS capture remains 20 ms. A server-free channel test runs the actual
  `start_teamspeak` pipeline and checks its encoded packet duration.
- The TS puppet pairs native single-frame 10 ms packets. Equal payload lengths
  use RFC 6716 code 1; unequal lengths use code 2, including the two-byte length
  encoding for lengths 252–1275. Frame payloads are copied without transcoding.
  TS-to-Gwar is unchanged; other existing browser/TS packetizations pass through.
- An odd tail is flushed before an end marker, after 40 ms without its partner,
  before another packet duration or TOC, and on graceful puppet shutdown.
  A zero-length second frame requests 10 ms of Opus concealment, completing a
  20 ms TS packet-counter step without losing the real tail. TOC changes cannot
  be merged; the old tail is completed and the new one held separately. This
  can add 10 ms of concealed audio at a mode/bandwidth/stereo transition.
  Repacketization assumes the SFU-delivered packet order; it does not recover
  upstream losses. Existing non-single-frame 10 ms browser packets are not
  normalized by this native-specific adapter.
- `audiopus` 0.3's repacketizer wrapper loses the output byte count and does not
  encode retained-input lifetimes. The bridge instead has a small bounded
  code-1/code-2 writer. Tests feed it into libopus's packet parser and decoder,
  checking boundary lengths and sample-for-sample equality against separately
  decoded frames. No production codec dependency was added to the server.
- SFU slot time already advances by the previous packet's actual duration.
  Pause detection retains at least the old 40 ms scheduling tolerance, so a
  delayed 10 ms callback batch does not invent lost RTP media. Regressions cover
  that case, 10/20/10 ms changes and a one-second pause. Source-generation changes still reset decoder, queued
  packets and staged PCM at native receivers.
- Receive target minimum and RTP loss concealment follow packet duration:
  480 samples for 10 ms, 960 for 20 ms. Starvation uses a 100 ms concealed-audio budget, checked at packet boundaries
  (ten 10 ms or five 20 ms frames); the timeline survives starvation.
  TS synthetic time still advances 960 per packet, independently of decoded
  duration. The target ceiling remains 160 ms; fractional trims remain at most
  5 ms, with the existing five-second low-water window and 2.5 ms safety reserve.
  Speaker buffering remains callback-sized, independent of encoder duration.
- Actual str0m offer/answer tests confirm `minptime=10;useinbandfec=1`, with no
  `a=ptime` or `a=maxptime`. The web engine does not rewrite those parameters;
  browser senders are unchanged. [RFC 7587](https://www.rfc-editor.org/rfc/rfc7587.html)
  permits 10 ms SILK, hybrid and CELT packets and treats `ptime` as a preference.
  This is source/SDP and codec evidence, not a Chrome/Firefox/Safari playback run.
  No browser or official TS client/server was run for this change.

## FEC and quality boundary

The local build statically links Homebrew libopus 1.6.1. A regression uses the
production bitrate/FEC/loss settings with a synthetic voiced signal, explicitly
labelled `Signal::Voice` and constrained to wideband or fullband to exercise
SILK and hybrid respectively. Both 10 ms modes recovered a missing frame using
LBRR: at least one of four tested losses decoded differently from PLC; controls
with FEC disabled produced PLC for all four. Successful `decode_fec` alone would
not establish this, because libopus falls back to PLC if no redundancy exists.
Production retains automatic mode selection; CELT has no SILK LBRR, and silence
or content-dependent decisions need not emit FEC on every packet.

The bundled libopus source accepts 10 ms SILK frames; its encoder forces CELT
only below 10 ms. See also [RFC 7587's FEC description](https://www.rfc-editor.org/rfc/rfc7587.html#section-3.3).
Native receive still uses immediate PLC, as before; this change does not add a
lookahead wait or start consuming LBRR there. Browser decoders can use it.
Repacketization tests establish lossless compressed-payload preservation, not
subjective speech quality. Physical-device, real-speech listening and WAN loss
acceptance remain separate from the synthetic loopback measurements.

## Bandwidth

Nominal active-stream rates, decimal kbps, with 48,000 bit/s of Opus payload
(including codec FEC allocation). VBR/content can change actual payload bytes.
One Opus frame per RTP packet gives 50 or 100 packets/s and nominal payloads of
120 or 60 bytes. No Ethernet/Wi-Fi, TURN, IP options, RTCP, ICE or DTLS handshake
traffic is included. Rates are per hop, per stream; SFU fanout multiplies them.

The two-byte generation value is not two bytes on the wire: its one-byte RTP
extension element header plus the shared four-byte extension header and padding
cost eight bytes if it is the only extension. With str0m's negotiated absolute
send time (four bytes including its element header) and TWCC (three), the native
downlink extension block is 16 bytes. A three-character MID adds four bytes until
RTCP acknowledges the SSRC. The normal native uplink's audio-level element takes
two bytes instead of the generation element's three and has the same padded
16-byte total with absolute send time and TWCC.

Formula: `48 + packets_per_second * 8 * overhead_bytes / 1000`.
RTP base is 12 bytes, UDP 8, IPv4 20 or IPv6 40. GCM SRTP adds 16 bytes per
packet; SHA1-80 SRTP adds 10 (subtract 2.4/4.8 kbps from the 50/100 pps GCM rows).
These are explicit negotiated-extension/profile budgets, not packet captures.

| Per-stream budget | 20 ms / 50 pps | 10 ms / 100 pps |
| --- | ---: | ---: |
| Opus payload only | 48.0 | 48.0 |
| IPv4 + UDP + RTP + generation-only extension + GCM SRTP (64 B) | 73.6 | 99.2 |
| IPv4 + normal 16 B extension block + GCM SRTP (72 B) | 76.8 | 105.6 |
| IPv6 + normal extension block + GCM SRTP (92 B) | 84.8 | 121.6 |
| IPv4 normal + SFrame 12 B header + 16 B tag (100 B) | 88.0 | 128.0 |
| IPv4 normal + maximum SFrame 17 B header + 16 B tag (105 B) | 90.0 | 132.0 |

The [E2E design](e2e.md) uses `AES_128_GCM_SHA256_128`, a 16-byte tag, and a
variable-length key id/counter header. Per [RFC 9605](https://www.rfc-editor.org/rfc/rfc9605.html#section-4.3),
the header is 1–17 bytes. The 12-byte example is one configuration byte, an
eight-byte key id and a three-byte counter; small epochs/counters can use fewer.
Thus SFrame adds 6.8–13.2 kbps at 50 pps or 13.6–26.4 kbps at 100 pps.
The table's SFrame cost is additional to SRTP, not a replacement for it.

At the normal IPv4/GCM budget, 10 ms costs another 28.8 kbps (37.5%). With the
12-byte SFrame header it costs 40 kbps more (45.5%), and doubles packet-processing
rate. Proposal: expose a low-latency setting and retain 20 ms for bandwidth-
constrained/self-hosted deployments; this change enables 10 ms through the single
constant but does not implement a setting or change the product UI.

## Local verification and measurements

Measured on macOS arm64 with Rust 1.99.0 and libopus 1.6.1, against a baseline
build of `a0f5686`. Each binary embeds its corresponding local server. Release runs were sequential, with 80 markers/run:
five idle, two with 14 owned CPU-busy processes, and one with
`LATENCY_CALLBACK_JITTER=1`, per build. No builds/tests ran alongside measurements.

Pooled empirical nearest-rank quantiles, in milliseconds. Counters are summed
across runs and slots, including warmup. PLC counts events, whose duration differs
between 10 and 20 ms streams. Trims count events; shrunk samples use 48 kHz.

| Condition | Build | Matched | Median | p95 | p99 | PLC | Trims | Shrunk samples | Underruns |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Idle | 20 ms baseline | 400/400 | 48.500 | 79.715 | 83.843 | 4 | 5 | 1198 | 0 |
| Idle | 10 ms candidate | 397/400 | 41.412 | 48.111 | 51.225 | 1 | 8 | 1271 | 0 |
| 14 busy processes | 20 ms baseline | 160/160 | 60.808 | 69.134 | 70.346 | 2 | 5 | 960 | 0 |
| 14 busy processes | 10 ms candidate | 160/160 | 47.364 | 50.238 | 51.456 | 2 | 0 | 0 | 0 |
| Variable callbacks | 20 ms baseline | 80/80 | 90.203 | 98.663 | 103.373 | 1 | 3 | 600 | 0 |
| Variable callbacks | 10 ms candidate | 80/80 | 75.410 | 80.504 | 84.977 | 0 | 1 | 120 | 0 |

All final runs passed the harness threshold. Candidate idle runs had three
unmatched markers (397/400); that is not evidence of three dropped network
packets. All other groups matched every marker. Active-stream underrun samples,
late drops and concealments within one second after a trim were zero in every
final run. These are synthetic ring-to-ring measurements, not physical
mouth-to-ear or perceptual-quality acceptance.

The median improvement was 7.088 ms idle, 13.444 ms under load and 14.792 ms
with variable callbacks. The small sample of calls and startup variability
limit generalization; use the per-run numbers below, not only pooled tails.

### Per-run results

| Condition | Run | Build | Matched/80 | Median | p95 | p99 | PLC | Trims | Underruns |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Idle | 1 | baseline | 80 | 47.903 | 50.173 | 50.736 | 0 | 0 | 0 |
| Idle | 2 | candidate | 80 | 43.079 | 47.753 | 48.661 | 0 | 2 | 0 |
| Idle | 3 | candidate | 79 | 42.130 | 48.935 | 50.188 | 0 | 2 | 0 |
| Idle | 4 | baseline | 80 | 47.504 | 50.142 | 50.723 | 1 | 0 | 0 |
| Idle | 5 | baseline | 80 | 66.459 | 69.437 | 71.507 | 1 | 1 | 0 |
| Idle | 6 | candidate | 80 | 37.219 | 39.708 | 40.631 | 0 | 0 | 0 |
| Idle | 7 | candidate | 79 | 43.880 | 51.225 | 55.149 | 1 | 2 | 0 |
| Idle | 8 | baseline | 80 | 74.733 | 83.843 | 86.055 | 2 | 4 | 0 |
| Idle | 9 | baseline | 80 | 47.202 | 49.339 | 50.646 | 0 | 0 | 0 |
| Idle | 10 | candidate | 79 | 42.139 | 48.111 | 50.388 | 0 | 2 | 0 |
| 14 busy processes | 1 | baseline | 80 | 60.594 | 69.210 | 70.346 | 1 | 3 | 0 |
| 14 busy processes | 2 | candidate | 80 | 47.464 | 49.732 | 50.558 | 1 | 0 | 0 |
| 14 busy processes | 3 | candidate | 80 | 47.242 | 50.483 | 51.677 | 1 | 0 | 0 |
| 14 busy processes | 4 | baseline | 80 | 61.472 | 69.088 | 71.053 | 1 | 2 | 0 |
| Variable callbacks | 1 | baseline | 80 | 90.203 | 98.663 | 103.373 | 1 | 3 | 0 |
| Variable callbacks | 2 | candidate | 80 | 75.410 | 80.504 | 84.977 | 0 | 1 | 0 |

### Open acceptance work

Real TS/browser playback, physical-device speech listening, WAN loss and longer
calls; also investigate the three unmatched idle markers before treating this
as a perceptual-quality result. The bandwidth-driven low-latency setting
proposal remains unimplemented.
