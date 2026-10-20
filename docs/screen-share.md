# Screen sharing (design)

Status: **design for review**. Nothing here is built yet. Phase 1 goes through
the server's SFU. Peer-to-peer sharing comes later and is only sketched at the
end. The spike code was thrown away; only its results are kept here
([Spikes](#spikes)).

## Summary

- **One media path for every client that has a usable webview.** Browsers, and
  the desktop app on macOS and Windows, capture with `getDisplayMedia` and send
  through extra `RTCPeerConnection`s in the web layer, the **screen
  transports**:
  - one *publish* transport per share;
  - one *watch* transport per viewer, with 4 fixed receive slots.

  The desktop's voice stays native and does not change. The spike confirmed
  that the system WKWebView at `tauri://localhost` has `getDisplayMedia`,
  `RTCPeerConnection`, `RTCRtpScriptTransform` and the VP8, VP9, H.264 and AV1
  encoders. WebView2 is Chromium. Linux desktop is the exception: distro
  WebKitGTK ships without WebRTC, so Linux users share from a browser until a
  native path exists (phase 3).
- **The SFU forwards video like it forwards voice.** It uses str0m 0.24 in RTP
  mode and does not decode. Spikes (Chromium/WebKit → str0m RTP-mode SFU →
  Chromium/WebKit) showed:
  - viewer NACKs are answered hop by hop from str0m's send buffer;
  - publisher losses are repaired by the SFU's own NACKs;
  - PLI relay brings late joiners in;
  - TWCC feedback and the send-side bandwidth estimate work;
  - 1080p60 forwards cleanly.

  Forwarding needs a **stable source-sequence mapping**, not arrival-order
  renumbering: renumbering corrupted about a quarter of the frames under
  publisher loss and reordering (S5).
- **Video does not run in the voice loop.**
  - The voice task stays the only reader of the existing UDP port. It hands
    known screen addresses to a separate screen task through an O(1) lookup
    and a bounded, non-blocking channel.
  - All video egress, retransmission and pacing happens in the screen task.
  - Paired voice latency measurements under video load and on constrained
    links gate the server PR. A separate task alone does not prove zero voice
    impact.
- **Codec: VP9 first, VP8 as the fallback, in every channel.** Encrypted
  channels use SFrame. The spikes settled which byte layouts survive the
  browsers' packetizers:
  - VP9 survives whole-frame SFrame.
  - VP8 needs its first 10 bytes (keyframe) or 3 bytes (delta) clear; they go
    into the AAD.
  - H.264 and AV1 send nothing at all when the whole frame is encrypted.

  H.264 with hardware encoding (games) comes later, with NAL-aware SFrame.
- **Keys** come from the existing voice call group:
  `MLS-Exporter("gwar screen v1", u32be(leaf), 32)`. The call controller gains
  a second endpoint kind (screen beside audio), with explicit rules for
  barriers, activation, restarts and nonce safety. Keyframe recovery covers
  viewers that switch keys late. Only call participants can watch.
- **Phase 1 is single-layer.** The sharer adapts only to its own uplink.
  - Each viewer's estimate is split across its watched shares in priority
    order (the stage first).
  - Shares that do not fit are paused for that viewer and never slow the
    sharer down.
  - Server resources are reserved by bitrate *cap* and enforced continuously.

  Temporal layers, then simulcast, come in phase 2.
- **Permission:** a new channel permission `share_screen`, which needs
  `connect`. Moderators with `mute_members` can stop someone's share. Limits
  are per channel, per share and per server.
- **TeamSpeak users** cannot see shares. TS3 has no video. In bridged
  plaintext channels they see a "sharing screen" marker in the channel
  description (phase 2).

## Goals and non-goals

Goals:

- Share a screen, a window or (in Chromium) a tab into one's voice channel.
- Everyone in that channel's voice can open the share. A share costs bandwidth
  only for people who open it.
- Low latency, for games and live coding. Target: p50 glass-to-glass at most
  150 ms over the internet at 1080p30, and at most 100 ms within a region.
- End-to-end encrypted in encrypted channels, with the same guarantees and
  limits as voice.
- Works on the deployment VM without transcoding.

Not in phase 1:

- peer-to-peer;
- camera video (the same transport would carry it later);
- recording;
- the shared screen's audio (phase 2);
- remote control;
- shares outside voice (for example "go live" to a text channel).

## 1. Capture per platform

### Browsers

| | Chrome / Edge | Firefox | Safari |
| --- | --- | --- | --- |
| `getDisplayMedia` | 72+ | 66+ | 13+ |
| Surfaces | tab, window, screen | window, screen | window, screen (macOS `SCContentSharingPicker`) |
| Shared audio | tab audio everywhere; system audio on Windows when sharing a screen (macOS: tab audio only, system audio unconfirmed) | none | none |
| `contentHint` | yes | no | yes (whether its encoder honors it is unconfirmed) |
| `CaptureController`, `surfaceSwitching`, `selfBrowserSurface`, Region/Element capture | yes | no | no |
| `RTCRtpScriptTransform` (for E2E) | 141+ (the spike ran 153) | 117+ | 15.4+ |
| Mobile | Android: no | Android: no | iOS 27: yes (MDN, 2026-10) |

Sources: MDN browser-compat-data (`MediaDevices.json`,
`MediaStreamTrack.json`, `RTCRtpScriptTransform.json`), Chrome developer docs
for Capture Handle, Region Capture and Element Capture, and the WebKit
`ScreenCaptureKitSharingSessionManager.mm` picker modes. The spike rows are
measured (see [Spikes](#spikes), S1).

`getDisplayMedia` needs a user gesture and a focused document. Both WebKit and
Chromium enforce this; spike S2 hit both errors. So the Share button calls it
directly in its click handler, with no `await` before the call.

Chromium constraints Gwar passes:

- `selfBrowserSurface: "exclude"`;
- `surfaceSwitching: "include"`, so the user can switch what they share
  without stopping;
- `systemAudio: "exclude"` while shared audio is not supported;
- a `CaptureController` with `setFocusBehavior("no-focus-change")`, so sharing
  a window does not pull it in front of Gwar.

Other browsers ignore unknown dictionary members, so the same call works
everywhere.

### Desktop app (Tauri 2.12, wry 0.57)

| Platform | Webview capture | Webview WebRTC | Decision |
| --- | --- | --- | --- |
| macOS 14+ | `getDisplayMedia` exposed at `tauri://localhost` (secure context, spike S2) | `RTCPeerConnection` and `RTCRtpScriptTransform` present; encoders VP8, VP9, H.264, H.265, AV1 (S2) | **Webview path** |
| Windows 10/11 | WebView2 = Chromium: `getDisplayMedia` with Chromium's picker; host can veto via `ScreenCaptureStarting` | yes | **Webview path** |
| Linux | portal/PipeWire capture works with wry's `DisplayCapture` permission kind | **no**: `enable-webrtc` defaults off; in 2.52/2.54 the GStreamer backend is incomplete and libwebrtc is not in release tarballs (WPE FAQ; WebKit bug 274711 closed 2026-07) | **Not in phase 1**; browser instead; native path in phase 3 |

macOS details:

- WebKit routes a display-capture request to its own
  `promptForGetDisplayMedia`, which uses `SCContentSharingPicker` on macOS 14+,
  when the host does not implement the private
  `_webView:requestDisplayCapturePermissionForOrigin:…` delegate. wry
  implements only the public camera/microphone delegate; with no handler set,
  it grants (`wry_web_view_ui_delegate.rs`).
- So no new entitlement should be needed: the picker is the consent. Two
  things are **not verified**:
  - the full prompt in a signed Gwar build (the spike could not focus a window
    from a background process; S2);
  - the open wry issue #1195, where a maintainer suspects the camera/microphone
    usage strings are also needed.
- PR 1 settles both by hand on macOS 14, 15, 26 and 27.
- The bundle's `minimumSystemVersion` stays 11.0. Below macOS 14 the Share
  button says "Screen sharing needs macOS 14 or later".

Windows details:

- The picker fails when the webview is under 600 px tall (WebView2Feedback
  #5173). The main window's `minHeight` is already 600.
- There is an open picker-close deadlock (#5763), to be watched.
- wry maps no screen-capture permission on WebView2. The default, which is
  Chromium's picker, is what we want.

### Why not native capture now

Native capture means:

- ScreenCaptureKit and VideoToolbox on macOS;
- Windows.Graphics.Capture and Media Foundation on Windows;
- xdg-desktop-portal, PipeWire and VA-API on Linux;
- packetizing with str0m's frame mode in `gwar-client`.

It gives zero-copy capture, hardware H.264/HEVC encoding and capture that
survives the webview being throttled in the background. It is also a second
encoder, packetizer, pacer, bandwidth controller and SFrame-for-video
implementation, in Rust, per platform. Estimate: 6–10 weeks for three
platforms. The crates exist but vary in maturity:

- `screencapturekit` 11 is active;
- `windows-capture` 2 is active;
- `ashpd` and `pipewire` are mature;
- `scap` is young;
- encoders are best reached through `ffmpeg-next` (LGPL/GPL to review) or
  thin, immature VideoToolbox/NVENC/VA-API crates;
- `openh264` is covered by Cisco's patent license only with the downloaded
  Cisco binary.

The webview path reuses one TypeScript implementation (capture, transport,
SFrame worker, UI) for the browser and two of three desktop platforms. It also
keeps the desktop's latency-critical voice engine untouched.

Tradeoffs we accept with the webview path:

- **Background throttling.** macOS may throttle a hidden WKWebView. This
  matters for sharing a fullscreen game while Gwar is minimized. Tauri windows
  are not App Nap'd while audio plays (the native engine plays), but this is a
  PR 1 check: frame rate with Gwar hidden for five minutes. If it fails, the
  desktop sets `NSProcessInfo.beginActivity(.userInitiated)` while sharing.
- **No hardware H.264 under E2E** in phase 1; see the codec section.
- **No system audio on macOS from the webview** (phase 2 uses native capture
  for that on desktop).

Phase 3 brings native capture: Linux first (no alternative there), then macOS
and Windows for games if measurements show the webview's encoder or throttling
falls short.

## 2. Codecs and encoding

### Support

| Codec | Chrome send/recv | Firefox | Safari / WKWebView | Hardware encode (typical) | SFrame on whole frame (spike S3) |
| --- | --- | --- | --- | --- | --- |
| VP8 | yes | yes | yes | rare | **breaks**: depacketizer cannot see the keyframe → PLI storm, 0 frames decoded. Works with 10/3 clear bytes |
| VP9 | yes | yes | yes (on by default) | Intel/NVIDIA on Windows; none on Apple | **works** (keyframe and size are in the clear RTP payload descriptor) |
| H.264 | yes (HW where available) | yes | yes (VideoToolbox) | everywhere | **breaks**: packetizer sends nothing. Works with NAL headers clear (LiveKit also escapes start codes) |
| AV1 | yes (screen-content tools) | 136+ | yes (decode needs M3/A17+) | rare (newest GPUs) | **breaks**: packetizer sends nothing; needs OBU-aware SFrame (LiveKit: unsupported) |
| H.265 | 136+, HW only | no | yes | Apple, recent GPUs | not tested |

**Phase 1 codec rule:**

- Prefer **VP9 (profile 0)**, then **VP8**, in plaintext and encrypted channels
  alike.
- One codec rule keeps the SFU's per-codec code to two RTP payload descriptors.
  VP9's descriptor also carries the temporal layer id that phase 2 uses.
- The sender sets `setCodecPreferences([VP9, VP8, rtx])`. The viewer offers the
  same list. The SFU writes the publisher's codec to each viewer, and every
  supported engine decodes both.

H.264 with hardware encoding is the obvious next step for 1080p60 game sharing
on weak CPUs. For encrypted channels it needs NAL-aware SFrame: everything up
to and including the first slice NAL header stays clear, and the ciphertext is
start-code escaped, as LiveKit does. The spike's unescaped variant decoded 204
of 204 frames in Chromium and 272 of 272 in WebKit. That was luck, not
robustness: about one frame in 2^24 bytes of ciphertext forms a start code.
This is phase 2 work. Plaintext H.264 could ship earlier, but one codec
everywhere is simpler to test.

### Presets

The user picks a preset. "Auto" is the default: Text for windows and tabs,
Smooth for screens; Chromium's `displaySurface` setting tells which.

| Preset | Resolution cap | fps cap | `contentHint` | `degradationPreference` | max bitrate |
| --- | --- | --- | --- | --- | --- |
| Text (code, documents) | 1080p (native if smaller) | 15 | `detail` | `maintain-resolution` | 2.5 Mbps |
| Smooth (default for screens) | 1080p | 30 | `detail` | `balanced` | 4 Mbps |
| Game | 1080p | 60 | `motion` | `maintain-framerate` | 8 Mbps |
| Saver | 720p | 30 | `detail` | `balanced` | 1.5 Mbps |

Notes on the table:

- Resolution is capped with `track.applyConstraints({width, height,
  frameRate})` on capture, never upscaled.
- Bitrate goes in `sender.setParameters({encodings: [{maxBitrate,
  maxFramerate}]})`.
- libwebrtc maps `detail`/`text` to maintain-resolution, turns off quality
  scaling, and enables VP9 screenshare tools. `motion` maps to
  maintain-framerate.
- The server limits the bitrate per share (a server setting, default 8 Mbps)
  and the SFU enforces it as a ceiling ([SFU](#3-the-sfu)).

Measured at the spike's 4 Mbps cap with synthetic screen content (scrolling
text plus a moving block) and software encoders on an M5:

| Codec / size | Viewer rate |
| --- | --- |
| 720p30, VP9 | 2.0 Mbps |
| 720p30, VP8 | 2.7 Mbps |
| 720p30, H.264 | 1.2 Mbps |
| 1080p30, VP9 (6 Mbps cap) | 4.8 Mbps |
| 1080p60, VP9 (8 Mbps cap) | 6.1 Mbps |

The largest keyframe was 204 KB at 1080p60, about 175 packets in a burst.

### Latency budget

| Stage | Typical |
| --- | --- |
| Capture (frame interval + compositor) | 8–33 ms |
| Encode | 2–15 ms (spike: VP9 3.6–5 ms, H.264 SW 5–15 ms, AV1 2.3 ms at 720p) |
| Pacing (keyframe bursts) | 0–20 ms |
| Network, sharer → SFU → viewer | RTT-dependent, 10–60 ms in region |
| SFU | < 1 ms: no reordering buffer; the pacer queue is normally empty and is capped at 150 ms ([Queues](#queues)) |
| Jitter buffer | 5–50 ms (spike: 5–53 ms on loopback) |
| Decode + render | 2–20 ms (one display refresh) |

Spike S3 measured p50 **34–108 ms** on loopback through the SFU at 720p30,
with a barcode stamped into the frames. It measured 57 ms at 1080p30 VP9 and
78 ms at 720p with 5 % loss. That covers capture-to-render, without a real
network or display. The machine was shared with two Rust builds (load average
about 10), which produced the p95 outliers.

Viewers set `receiver.jitterBufferTarget = 0`, which Chromium and Safari 27
support, to keep the buffer minimal. The 150 ms internet target is realistic
with this budget.

## 3. The SFU

### What str0m 0.24.1 gives in RTP mode

From the source in `~/.cargo/registry/.../str0m-0.24.1` and confirmed by spike
S3:

- **Video codecs:** VP8, VP9, H.264, H.265, AV1 (`enable_*`). RTP mode bypasses
  str0m's packetizers, so the SFU sees and writes raw RTP payloads, as it does
  for Opus today.
- **NACK/RTX:**
  - Incoming RTX is unwrapped to the main SSRC (`session.rs`, `un_rtx`).
  - NACKs are generated for gaps when RTX is negotiated.
  - Outgoing packets written with `RtpWrite::nackable(true)` are answered from
    the send buffer (`set_send_buffer_video`, default 1000 packets).
  - In S3 with 5 % loss toward viewers, each viewer sent 130–183 NACKs and the
    publisher received **0**: retransmission is hop-by-hop, so one lossy
    viewer does not reach the sharer.
- **PLI/FIR:**
  - `Event::KeyframeRequest` arrives from viewers.
  - `StreamRx::request_keyframe` asks the publisher.
  - S3's late joiner decoded after one relayed PLI.
- **TWCC:** str0m sends transport-wide feedback for incoming streams, so the
  browser's own congestion control on the sharer's uplink works.
- **Send-side BWE:** `enable_bwe` and `Event::EgressBitrateEstimate` give an
  estimate per viewer. In S3 the estimates reached 2–12 Mbps on loopback.
- **REMB:** `StreamRx::request_remb` exists (not used in phase 1).
- **Simulcast:** RID streams. `RtpWrite::vp8_patch` rewrites VP8 picture ids for
  layer switching (phase 2).

One str0m constraint shapes the loop. In RTP mode, `Rtc` keeps **one pending
packet** between `handle_input` and `poll_output` (`session.rs`,
`pending_packet`). A second input before polling overwrites the first. The
spike hit this (S5): repaired packets vanished until the loop polled after
every input. `gwar-server`'s voice loop already handles one datagram and then
drives the peer. The screen task must do the same.

### Transports

Each session has at most two screen transports besides its voice transport
(a session shares at most one thing at a time):

- **Watch transport.** One `Rtc` per session that watches anything, negotiated
  by `screen.watch.offer` on the first watch and kept while the session is in
  the call. Its offer is fixed: `SCREEN_SLOTS = 4` `recvonly` video m-lines,
  nothing to send. This mirrors the eight audio slots: no renegotiation when
  shares start, stop or change hands. A `run` number restarts it, like voice's
  run.
- **Publish transport.** One `Rtc` **per share**, negotiated by
  `screen.publish.offer` and closed when the share ends: one `sendonly` video
  m-line with RTX.
  - A new share is a new `RTCPeerConnection`. It gets a fresh DTLS session and
    fresh SSRCs from the browser by construction. `replaceTrack` on a kept
    transceiver would keep the old SSRC.
  - A delayed packet of a previous share belongs to a closed `Rtc`, and the
    dispatcher drops it before any parsing.
  - Starting costs one ICE-lite check plus a DTLS handshake (2–3 RTTs, about
    100–200 ms in region) before the first frame. That is acceptable for
    starting a share.
  - Watching other shares is unaffected, because it runs on the separate
    watch transport. PR 4's e2e covers start, stop and start while watching
    two other shares.
- **Codecs:** `clear_codecs().enable_vp9(true).enable_vp8(true)`, with RTX.
  Extensions are the defaults (transport-cc, abs-send-time, mid, rid) and a
  slot-generation extension on the watch transport like voice's
  (`gwar_proto::rtp`).

### Socket ownership and dispatch

The voice task stays the only reader of the media UDP socket. Screen
datagrams reach the screen task without slowing voice down.

- **Dispatch is O(1) and happens first.** The voice task keeps a
  `HashMap<SocketAddr, Owner>`, where the owner is a voice peer or a screen
  `Rtc` id. It is filled when an `Rtc` first accepts an address, and cleared
  when that `Rtc` closes.
  - A datagram from a known screen address goes over a bounded channel
    (`try_send`, capacity 2048 datagrams) to the screen task. Nothing else
    happens to it in the voice task: no peer scan, no `handle_input`, no
    drive.
  - Unknown addresses (new ICE checks) fall back to asking voice peers'
    `accepts`, then the screen task's.
  - `accepts` classifies by remote address and STUN credentials (ICE
    ufrag/pwd). This is **routing, not authentication**: SRTP authentication
    in the owning `Rtc` is what rejects forged media, as for voice today.
- **The hand-off never blocks.** A full channel drops the datagram and counts
  `screen_dispatch_dropped`. Video recovers through NACK/PLI; voice never
  waits.
- **Egress belongs to the screen task.** It sends through a clone of the
  socket handle (`Arc<UdpSocket>`, `try_send_to`, dropping under socket
  pressure like voice). Fan-out, retransmissions and padding probes run there,
  on its own Tokio task. Phase 1 uses the same runtime. If PR 3's measurement
  shows voice jitter, PR 3 moves it to its own thread.
- **What this does not isolate:**
  - the kernel's socket buffers and the NIC queue, which are shared;
  - the clients' uplinks and downlinks, where audio and video congestion
    controllers compete (next section).

  A separate UDP port would isolate the socket buffer but not the rest. It
  stays an option (`--screen-media`) if the measurements below demand it.

### Voice headroom and acceptance

- **On the sharer's uplink,** audio and video are separate peer connections
  with separate congestion controllers. Browser GCC is delay-based and backs
  off when the queue grows, which also protects audio. On top of that:
  - the video `maxBitrate` is the preset cap, never more;
  - the sharer lowers it to `0.7 × availableOutgoingBitrate` of the publish
    transport (from `getStats` `candidate-pair`) when that is lower, leaving
    30 % for voice and retransmissions;
  - video encodings get `priority: "low"` and `networkPriority: "low"`, and
    voice keeps the default. This is a DSCP hint that most paths ignore.
- **On a viewer's downlink** the SFU's per-viewer allocation (below) never
  plans more than 80 % of the viewer's estimate. Voice is about 32–40 kbps per
  speaker and fits in the remainder.
- **PR 3 acceptance compares voice with and without video,** paired runs, using
  the `latency` example and `LATENCY_NET` relay from docs/latency.md:
  - The relay gets a `rate=` bottleneck option with a bounded queue. The
    example gets a screen load generator: a str0m frame-mode client publishing
    synthetic VP9-sized frames at a preset's rate, through the same relay, and
    a viewer receiving it.
  - Scenarios: unconstrained loopback with a 1080p60 share at 8 Mbps and five
    viewers; a 10 Mbps sharer uplink with an 8 Mbps share; a 5 Mbps viewer
    downlink watching two 4 Mbps shares; each also with 1 % loss.
  - Report voice p50/p95/p99 mouth-to-ear, lost and late frames, playout
    underruns and concealment, as the latency docs do. Ten pairs, alternating
    order.
  - Gate: unconstrained, voice p95 and p99 within 2 ms of the paired baseline
    and no extra underruns. Constrained, voice p99 within 20 ms of baseline and
    underruns within baseline + 0.5 %. Missing the gate blocks PR 3 until the
    isolation is changed (own thread, separate port or a lower video
    allocation).

### Admission and resource limits

Each share's publish `Rtc` binds its negotiated SSRC and RTX SSRC to an
immutable `ScreenOrigin {session, share, channel, encrypted, private}`. This
is captured at negotiation and re-checked on every forward, like
`AudioOrigin`. Packets of other SSRCs drop.

Limits are enforced continuously, not only when a share or watch is admitted:

- **Per share, ingress.** Two token buckets, checked before forwarding:
  - bytes: rate 1.25 × the share's cap, burst 512 KB (two 1080p keyframes);
  - packets: rate 2,000 per second, burst 600.

  Payloads up to the negotiated MTU (1200 bytes) are allowed, instead of
  voice's 1033-byte cap. There is no codec parsing beyond the payload
  descriptor's start-of-frame and keyframe bits.
- **Per viewer slot.** A reservation equal to the share's **cap**, not its
  current rate. Static content cannot be admitted cheaply and then grow. Every
  reservation is charged 1.3 × its cap to cover RTX, TWCC padding probes and
  headers.
- **Server-wide budget** (default 300 Mbps). The sum of all slot reservations
  plus all publish caps, because ingress costs CPU too. A watch or start that
  would exceed it gets `busy`.
  - The screen task also measures real egress every second, including
    retransmissions and padding, from str0m's `Event::PeerStats`, enabled with `set_stats_interval(1 s)`.
  - When it stays over budget for 5 s, the newest watches are paused first.
- **Negotiation.**
  - At most 4 `screen.*.offer` per session per minute.
  - At most `shares_per_server` publish transports (default 32) and one watch
    transport per session.
- **Dropping is frame-granular.** When a bucket or the budget forces a drop,
  the SFU drops from the next frame start (a new RTP timestamp with the
  descriptor's start bit) onward. It marks the slot **waiting for keyframe**
  and forwards nothing to it until a keyframe arrives, which the throttled PLI
  rule below requests. No partial frame is ever forwarded.

### Forwarding: stable sequence mapping

Arrival-order renumbering is wrong. A publisher packet repaired by RTX, or
reordered on the way, arrives after its successors. Renumbering it in arrival
order makes viewers reassemble frames in the wrong order. S5 measured it:
95–98 corrupted frames and 2–7 decoded out of about 420, under 3 % publisher
loss and 2 % reordering, against 0 corrupted and all decoded with the mapping
below.

Each viewer slot keeps a **segment**: `(base_src, base_out)` and a
`resume_src`.

- str0m gives the publisher packet's **extended** source sequence number
  `src`. RTX repairs are unwrapped to it, `un_rtx`.
- Within a segment, `out = base_out + (src - base_src)`:
  - gaps are preserved, so the viewer NACKs them;
  - a late or repaired packet lands exactly on its original `out`, as a
    first-time packet;
  - retransmissions map consistently.
- A new segment starts at slot assignment, and when resuming after a pause or
  a frame-granular drop. It always starts at a keyframe's first packet:
  `base_src` is that packet's `src`, and `base_out` is the slot's highest
  `out` + 1 + 64. The jump makes the viewer treat the hole as an old loss, and
  the keyframe makes it irrelevant.
- Packets with `src < resume_src` are stale and dropped, so a repair from
  before a drop never reaches the new segment.
- Timestamps pass through with a per-segment offset, so a slot reused for
  another share starts a fresh timeline. The marker bit is copied, the payload
  type is mapped to the viewer's, and packets are written `nackable(true)`.

Viewer NACKs are answered hop by hop from str0m's send buffer. A gap the SFU
itself has not received yet stays open until the SFU's own NACK to the
publisher is repaired (S5: 72–96 publisher NACKs answered). The repair then
fills it at its mapped `out`. S5 decoded 412–423 of about 420 frames with no
freeze.

Retransmissions resend the same SFrame ciphertext under each viewer's own SRTP
keys. The SFU needs no screen keys for this, and nothing about it weakens the
end-to-end layer.

### Queues

Writes do queue: `write_rtp` puts packets into str0m's send queue. With BWE
enabled, a leaky-bucket pacer drains it. The pacer's 2 s queue limit makes it
*send faster*, not drop (`pacer/leaky.rs`). The queue cannot be cleared from
outside str0m. So the SFU controls what goes in:

- before writing a frame's first packet to a slot, it reads the stream's
  `queue_info()`;
- when the oldest unsent packet is more than 150 ms old, or more than 256 KB
  are queued, it drops that frame for that slot and the slot waits for a
  keyframe, as above;
- packets already queued drain at pacing rate, which is the bounded leftover
  of at most 150 ms or 256 KB.

On pause, the slot stops receiving writes immediately, and the residue drains
within that bound. Unpaced streams (`set_unpaced`) are not used for video.

### Keyframes

All keyframe requests to a publisher go through one throttle per share: at most
one PLI per 500 ms, with later requests in the window coalesced. Inputs are:

- a slot assignment (watch, retry, resume after pause);
- a viewer's PLI/FIR;
- `screen.keyframe` from a viewer's decryptor;
- a slot waiting for a keyframe after a drop.

Each viewer may contribute at most 10 per minute, whatever the source.
Watch, retry and `screen.keyframe` cannot bypass the throttle; past the
budget, the viewer stays paused until the next keyframe the share produces
anyway. This is what S3's whole-frame VP8 case looked like: 94 viewer
requests in 10 s.

### Bandwidth allocation per viewer

The watch transport carries up to 4 slots over one TWCC estimate, so decisions
are made for the **sum**:

- the SFU sets `bwe().set_desired_bitrate(Σ caps of watched shares × 1.1)`,
  which is what str0m's BWE API asks for when several streams share a
  transport (`bwe/api.rs`);
- it allocates `0.8 × estimate` to the watched shares in **priority order**:
  1. the share on the viewer's stage (the client says which, with
     `screen.focus {share}`);
  2. then the other watched shares in watch order.

  Each gets its current measured rate (2 s average) while the remainder allows.
- a share that does not fit for 5 s is paused for that viewer (`screen.paused
  {share, reason: "bandwidth"}`), lowest priority first. A paused share resumes
  when the estimate allows it for 10 s: a new segment from a keyframe, with
  `screen.resumed`.
- the viewer sees "Your connection is too slow for this share" on a paused
  card, with a Retry button (throttled as above).

The sharer is never slowed by viewers. The SFU never relays viewers' estimates
or REMB to the publisher. The sharer adapts only to its own uplink (TWCC on
the publish transport).

Phase 2:

- **Temporal layers.** VP9 `L1T3` in Chromium; its descriptor TID is clear even
  under SFrame. A slow viewer gets 15 or 7.5 fps instead of a pause, with no
  keyframe needed. Firefox does not support `scalabilityMode`, and the
  Playwright WebKit build ignored it (S1), so these senders stay single-layer.
- **Simulcast** with two layers (full and half resolution), switched on
  keyframes. Browser support is uneven: Firefox has VP8/H.264/AV1 simulcast but
  no VP9; Safari has VP8/H.264. This needs codec-specific rules, so it comes
  after temporal layers.

### Cost on the deployment VM

Egress per viewer equals the share's rate (the SFU never transcodes):

| Preset | Typical per viewer | 1 hour per viewer |
| --- | --- | --- |
| Text | 0.3–1.5 Mbps (mostly static) | 0.1–0.7 GB |
| Smooth 1080p30 | 2–5 Mbps | 0.9–2.2 GB |
| Game 1080p60 | 5–8 Mbps | 2.2–3.6 GB |

- **Oracle's 10 TB/month of free outbound transfer** is about 5,500
  viewer-hours at 4 Mbps. For example, two shares a day, three hours each, five
  viewers each: 2 × 3 × 5 × 1.8 GB × 30 days ≈ 1.6 TB/month.
- **Network:** A1 shapes get 1 Gbps per OCPU, so bandwidth binds after CPU.
- **CPU, measured in S3** on an Apple M5, with str0m RTP mode doing SRTP for
  every packet:
  - marginal cost ≈ **0.15 % of one core per forwarded Mbps**: 6 viewers at
    2 Mbps used 3.3 % of a core, against 2.0 % for 1 viewer;
  - the spike's polling loop was unoptimized;
  - Neoverse-N1 cores are roughly 2–3× slower per thread, so assume **about
    0.4 % of an A1 core per Mbps**;
  - **100 Mbps of egress (25 viewers at 4 Mbps) ≈ 0.4 of one A1 core**, on a
    task separate from voice.
  - PR 3 re-measures this on the VM.
- **Compute allowance.** Oracle's Always Free page now gives every tenancy
  1,500 A1 OCPU-hours and 9,000 GB-hours a month, which is 2 OCPUs and 12 GB
  running continuously. It changed around June 2026 (InfoQ, 2026-07).
  - The VM was created as 4 OCPU / 24 GB. On a free-only tenancy the
    excess must be resized away; on a paid tenancy it is billed.
  - Which applies to this VM is **not verified** (open question 5).
  - 2 OCPU is enough for the phase 1 numbers above; the defaults below assume
    it.

### Limits (server settings, with defaults)

- Concurrent shares per channel: **4**, which equals `SCREEN_SLOTS`, so anyone
  can watch every share at once.
- Shares per server: **32**.
- Viewers per share: **25**.
- Server-wide screen budget, reservations as above: **300 Mbps**.
- Bitrate cap per share: **8 Mbps**.
- `screen.watch` from someone not in the share's voice call: `forbidden`.

## 4. Encryption

### Keys

A share lives inside a voice call, and the call's MLS group (5a) is the share's
group. For epoch E and the sharer's leaf L:

```text
base_key(E, L) = MLS-Exporter("gwar screen v1", u32be(L), 32)
kid(E, L)      = ((E & 0xffffffff) << 32) | L        (same layout as voice)
```

The label differs from voice's `"gwar voice v1"`, so a video frame can never
authenticate as audio or the reverse, and the two never share a nonce counter.

Rules shared with voice (e2e-build.md §5), applied per media kind:

- the suite (`AES_128_GCM_SHA256_128`), RFC 9605 header and derivation;
- one encryptor per `(call, group, full epoch, own leaf, media kind)`;
- the eight-epoch receiver table and retention, and removal purges;
- the 2^32 sender limit;
- fail closed: nothing is sent or shown without keys, and the stage shows
  "Securing screen share…".

Replay windows are per `(group, epoch, leaf, media)`. Video frames arrive in
order through the jitter buffer, so the 128-frame window is ample.

### Two engines under one call controller

Today `VoiceCall.attach(media)` (`apps/web/src/mls/voice.ts`) holds exactly one
`VoiceMedia` endpoint. That endpoint is either the browser's SFrame worker or
the desktop's native engine through `voice_set_keys`. The controller binds to
it:

- serials, `barrier`, `installed`, `activated` and `floor`;
- acknowledgements carry its `run`;
- `restartMedia()` replaces it.

On the desktop, audio stays native while video is a web worker, so one
endpoint cannot serve both. The controller therefore gets **endpoints by kind**:

- `attach(kind: "audio" | "screen", media)`. Each kind has its own run, serial,
  barrier, installed, activated and floor.
- **Attaching or restarting `screen` never touches the `audio` endpoint**, and
  the reverse. `restartMedia(kind)` replaces only that kind. A failing screen
  endpoint (control error, worker crash, install timeout) closes the screen
  transports and shows "Screen share stopped"; voice continues.
- **Acknowledgements stay audio-driven:** `voice.epoch.ack` is sent when the
  *audio* endpoint installed the epoch, exactly as today. Voice activation
  timing does not change when someone shares.
  - A viewer whose screen endpoint installs later than its audio endpoint can
    miss video frames briefly.
  - Recovery covers it (next section).
  - The cost of the alternative, making acks wait for video, would be voice
    activation delays caused by video.
- **Barriers and holds go to every attached endpoint.**
  - `supersede` is sent to each endpoint, and the controller's `barrier`
    advances per endpoint.
  - A `hold` (removal, revocation, gate closed) is applied to both. "At once"
    for a removal means each endpoint stops sending under the old key when it
    processes the hold.
  - An endpoint that does not confirm a hold within 1 s is closed: screen
    transports torn down, or the voice run restarted as today.
- **Installation and activation run per endpoint, with one decision.** The
  controller decides once per epoch whether it may activate, using the same
  rule as today: removal at once, otherwise all acks or two seconds. It then
  sends `install` and `activate` to each endpoint independently.
  - An endpoint that has not finished `install` simply activates later. Its
    sender stays silent until then.
  - No endpoint ever activates an epoch the decision has not reached.
- **Restart and nonce safety.** The screen encryptor's counter must never
  repeat under a base key. A new SSRC or a new transport does not protect the
  nonce: SFrame's nonce is derived from the key and the counter only.
  - **The worker outlives transports.** The screen worker belongs to the
    call participation, not to a publish or watch transport. Each new
    transport attaches new `RTCRtpScriptTransform`s to the *same* worker, so
    the encryptor keeps its counter across shares and transport restarts.
  - **A lost worker counts as a restart.** If the worker itself is lost
    (crash, `onerror`), the screen endpoint is a restarted engine under the
    voice rule. It gets a new floor and needs this device's own leaf update.
    It may send video only in the epoch of that exact update, or a later one,
    and never under a key from before it. The update is an ordinary
    non-removal commit: the audio endpoint installs it like any epoch, and
    voice keeps sending under its current key until normal activation.
  - The same holds for the desktop: the screen worker lives in the webview's
    MLS runtime; the native audio engine is a separate endpoint.
- **The desktop needs no new native command.** Video keys never cross into
  Rust. The webview's MLS runtime exports them straight to the screen worker.

The control contract (`hold`, `supersede`, `install`, `activate`) and the key
state module (`apps/web/src/voice/sframe/state.ts`) are reused unchanged, with
the media kind added to the key binding.

### Keyframe recovery around key changes

A viewer can receive the requested keyframe under epoch E. If the sharer then
activates E+1 before the viewer's screen endpoint has installed it, the viewer
drops the frames it cannot open. When it catches up, the following delta frames
authenticate but cannot be decoded: their references are gone (RFC 9605 §6.2).
So:

- The receiver worker tracks, per slot, whether any frame was dropped since
  the last keyframe it delivered. Reasons: `no_key`, `auth_failed`, replay,
  oversize.
- When a slot has such a gap, the first frame that opens again triggers
  `screen.keyframe {share}`. The same happens after a decoder reset (a `freeze`
  or `framesDropped` jump in `getStats`) and after a slot is reassigned.
- Retries happen at most once per second, while the gap persists, up to five
  times. After that the card shows "Reconnecting to the share…" and the viewer
  re-watches. All of this goes through the server's per-share throttle and the
  per-viewer budget, so it cannot amplify.
- **Sender side:** right after a sharer activates a new epoch, it asks its own
  encoder for a keyframe, through `generateKeyFrame()` on the worker's
  `RTCRtpScriptTransformer` (WebRTC Encoded Transform spec). Where an engine
  lacks it (PR 5 checks all three), the viewers' requests cover it. That
  keyframe is the first frame under the new key, so viewers that already have
  the key never see a gap.

### Frame layout

- **VP9:** `SFrameHeader || AEAD(frame)`, as voice does it.
- **VP8:** `clear || SFrameHeader || AEAD(rest)`, where `clear` is the first
  10 bytes of a keyframe or 3 of a delta frame (Jitsi and LiveKit use the
  same split). `clear` is the RFC 9605 *metadata*, so it is authenticated:
  AAD = `header || clear`. The web `seal`/`open` already take `metadata` for
  this (`apps/web/src/voice/sframe/suite.ts`).
- **Bounds.** Encrypted frames over 2 MiB are dropped before encryption. A
  1080p keyframe is about 200 KB.

The spike evidence for these layouts is **layout feasibility only**. S3 and S5
used a scrambling transform with a checksum, not AES-GCM. They show:

- which byte layouts the browsers' packetizers and depacketizers carry
  through the SFU;
- that corruption would be detected.

They do not measure real AEAD cost in the worker. PR 5 measures that: worker
time per frame at 1080p60 keyframe sizes, in Chromium, WebKit and Firefox.

Capability check: no `RTCRtpScriptTransform` means "Update your browser or use
the desktop app to watch or share in encrypted channels", the same wording
style as voice.

### What the server still sees

Everything it sees for voice, plus:

- who shares, when, and who watches whom;
- per frame: size, timing and the keyframe flag (VP9 descriptor P bit; VP8's
  clear bytes);
- the resolution (VP9 scalability structure on keyframes; VP8's clear keyframe
  header) and frame rate;
- bitrate over time. Frame sizes leak activity: a static document versus
  scrolling versus a game is easy to tell apart.
- RTP header extensions: abs-send-time, transport-cc, playout-delay, and
  Chromium's `video-content-type`, which marks screenshare.

None of the pixels. This list goes into e2e.md's "What the server still sees".

### Plaintext and TeamSpeak channels

- **Plaintext channels:** same transport and codecs, no transform. The server
  could read the video by depacketizing it.
- **TeamSpeak** has no video, so TS users never receive a share.
  - In a bridged plaintext channel, the bridge's presence line in the channel
    description gets " · sharing screen" after the sharer's name. The
    description already lists Gwar users (`Presence` in
    `teamspeak/bridge.rs`).
  - Encrypted channels are unreachable from TS anyway, and their description
    stays the fixed notice.
  - The puppet's nickname does not change: renames cost TS slots and spam
    the channel.
- **The desktop's TeamSpeak mode** (Gwar client on a TS server) offers no
  sharing.

## 5. Protocol and permissions

### Permission

New channel permission **`share_screen`** ("Share screen").

- Scope: channel, overridable per role and member. It is added to
  `Permission::CHANNEL`.
- Dependency: without `connect` it is off (the resolution rule in roles.md
  step 4).
- Default: in `MEMBER_DEFAULT` (open question 3).
- Watching needs no permission beyond being in the channel's voice. Anyone who
  can hear the channel can see its shares.

### Requests (`gwar/4`)

All are bound to the voice participation, `{call, participation}`. A request
after leaving the call answers `stale`.

| Op | Body | Reply / effect |
| --- | --- | --- |
| `screen.start` | `{call, participation, preset, surface}` (`surface`: `screen`/`window`/`tab`/`unknown`, display only) | `{share}`, a 16-byte random id; checks `share_screen`, limits and budget. Not announced until its publish transport connects |
| `screen.publish.offer` | `{share, ssrc, rtx_ssrc, sdp}` | `{sdp}`; the share's own publish transport. Its first connection announces `screen.started` to the channel. One per share, never renegotiated |
| `screen.stop` | `{share}` | ok; event `screen.stopped {share, reason: "stopped"}` |
| `screen.watch.offer` | `{call, participation, run, sdp}` | `{sdp}`; the session's watch transport (4 `recvonly` slots). `run` restarts it like voice's run |
| `screen.watch` | `{share}` | `{slot}`; needs a connected watch transport; the server assigns a slot, opens a segment at the next keyframe (throttled request), event `screen.slot` |
| `screen.focus` | `{share}` | ok; the share on the viewer's stage gets first claim on its bandwidth |
| `screen.unwatch` | `{share}` | ok; the slot is freed |
| `screen.keyframe` | `{share}` | ok; throttled PLI. Lets a viewer whose decryptor lost sync, for example after a key change, recover without re-watching |
| `screen.stop_other` | `{share}` | needs `mute_members` in the channel and rank above the sharer; event `screen.stopped {reason: "moderator", by}` |

Events:

- `screen.started {share, client, channel, preset, surface}`;
- `screen.stopped {share, reason: stopped|moderator|left|limit|transport|budget}`;
- `screen.slot {binding, slot, share | null}`, like `voice.slot`;
- `screen.paused {share, reason: bandwidth}` and `screen.resumed {share}`.

`Welcome` and `VoiceState` carry `shares: [{share, client, preset, surface}]`
for the channel one is in. Shares in other channels are not listed: phase 1
shows them only to the channel's voice. The channel tree shows a "LIVE" badge
next to sharers to everyone who can see the channel. That uses one more client
field, `Client.sharing: bool`.

Lifecycle rules:

- A share ends when the sharer stops, leaves or moves channel, disconnects,
  loses `share_screen` (overrides re-evaluate, like `speak`), the screen
  transport fails, or a moderator stops it.
- A share's publish transport closes with it; its SSRCs die with that `Rtc`.
- A new share always gets a new share id and a new publish transport, so no
  frame from an old share can enter a new one.
- Start, stop and start again while watching other shares touches only publish
  transports; the watch transport and its slots are unaffected.

TypeScript types are generated as today (`cargo test -p gwar-proto --features
ts`). The handwritten client validators gain the new schemas (architecture.md,
"Input bounds").

### Shared audio

Not in phase 1. In phase 2:

- one `sendonly` Opus stereo m-line beside the video, plus one `recvonly` audio
  m-line per screen slot;
- not speech-gated: no RFC 6464 gate, music must pass;
- SFrame under `"gwar screen audio v1"`.

Sources:

- Chromium tab audio, and system audio on Windows;
- desktop system audio through native capture: ScreenCaptureKit audio with
  `excludesCurrentProcessAudio`, WASAPI process loopback excluding Gwar,
  PipeWire. Gwar's own voice playback must never be shared back; Chromium's
  `suppressLocalAudioPlayback` / `restrictOwnAudio` covers the browser.

Firefox and Safari cannot share audio at all.

## 6. Interface

- **Starting.**
  - A "Share screen" button in the voice panel (`ui/VoicePanel.tsx`), next to
    mute and deafen. It is disabled with a tooltip when missing permission,
    unsupported (Linux desktop, macOS < 14, mobile Chrome/Firefox) or in an
    encrypted channel without transforms.
  - Click → the browser/OS picker → a small preset menu: Auto, Text, Smooth,
    Game, Saver.
  - The preset can change while sharing, with no renegotiation, through
    `setParameters` and `applyConstraints`.
- **While sharing.**
  - A red "You are sharing" bar with Stop (and Chromium's "Switch source"), at
    the top of the window, in the tray menu, and as a red dot on the tray icon
    on desktop.
  - The sharer's own preview is a small thumbnail, off by default, because a
    mirrored screen is confusing.
  - Browsers add their own indicators (Chrome's "Stop sharing" bar, macOS's
    menu-bar indicator).
- **Discovering.**
  - A "LIVE" badge next to the sharer in the channel tree and the voice panel.
  - Clicking it, or "Watch" on the share card, opens the share. Nothing
    streams before that.
- **Watching (the stage).**
  - The main content area switches from chat to a stage: the active share
    large, other shares in this channel as cards (click to swap), and chat
    collapsible on the right.
  - Controls: fullscreen (double-click or F), picture-in-picture
    (`video.requestPictureInPicture()`, Chromium and Safari; Firefox has its
    own PiP button on video), a quality badge (resolution/fps, from
    `getStats`), and "Stop watching".
  - The sharer's speaking ring shows on their card, from the existing
    `voice.talking` event.
  - Video volume does not apply in phase 1 (no audio).
- **Encrypted.** The same lock as voice ("End-to-end encrypted") on the stage,
  and "Securing screen share…" until keys are active. Channel info's "who can
  hear" also covers watchers: the same set as the call.
- **Desktop vs. browser.**
  - The same React UI.
  - The desktop adds the tray indicator, and later (phase 2) **pop-out**: the
    stage in its own native window (a second Tauri window loading the same
    bundle with a stage-only route; the capability file must grant that window
    only what it needs), for a second monitor.
  - Browsers get pop-out through Document Picture-in-Picture (Chromium 116+)
    in phase 2. Elsewhere, plain PiP.
- **TeamSpeak mode and Linux desktop:** no button, and a one-line explanation
  in the voice panel's menu.

Polish and English strings are added together, as for every UI change.

## 7. Phases and pull requests

Each PR merges on its own with CI green. Estimates are working days for one
person.

### Phase 1: sharing through the SFU

1. **Desktop capture check (S, 1–2 days).**
   - The desktop gets the screen-capture permission wiring: Tauri 2.12's
     permission handler (`tauri-runtime-wry` 2.12.1 passes it to wry) grants
     `DisplayCapture` for the `main` window only. If #1195's report holds, it
     also adds `NSCameraUsageDescription`, which needs care because the app
     does not use the camera.
   - The same handler **denies** webview microphone and camera. Today no
     handler is set, and wry then grants every public macOS media-capture
     request (`wry_web_view_ui_delegate.rs`). The desktop's audio is native,
     so the renderer never needs them; this narrows the native command
     surface table in architecture.md.
   - A debug-only page calls `getDisplayMedia` and reports settings and
     frame rate.
   - Manual acceptance: macOS 14, 15, 26, 27 (picker appears; capture works;
     no TCC prompt or the expected one; frame rate with the window hidden for
     five minutes) and Windows 10/11 (picker; frame rate).
   - Results are recorded in this document.
   - This settles the only unverified premise of the webview path before
     anything depends on it.
2. **Protocol, permission and share state (M, 3–4 days).**
   - `share_screen`;
   - the requests and events above, except the `*.offer` and slot ones;
   - limits, lifecycle and the moderator stop;
   - `Client.sharing`;
   - shared role-resolution vectors;
   - server tests in `tests/protocol.rs`, `tests/overrides.rs` and
     `tests/moderation.rs`;
   - the client validators;
   - no media yet.
3. **SFU video forwarding (L, 8–11 days).**
   - The address-map dispatch and bounded hand-off from the voice task, and
     the screen task.
   - `screen.publish.offer` and `screen.watch.offer`, and their `Rtc`s
     (VP9/VP8 + RTX), polling after every input.
   - Share admission: the `ScreenOrigin` binding and byte/packet buckets.
   - Slot assignment, `screen.watch`/`unwatch`/`focus` and `screen.slot`.
   - Stable sequence mapping with segments, and timestamp offsets.
   - Frame-granular drops with wait-for-keyframe, queue-age control, the one
     keyframe throttle, and NACK/RTX.
   - Per-viewer aggregate allocation with pause/resume.
   - Cap-based reservations, continuous egress measurement and negotiation
     limits.
   - Counters in `MediaCmd::Inspect`.
   - Tests: Rust end-to-end in `tests/media.rs` style, using str0m frame-mode
     clients as sharer and viewers:
     - late join decodes from a keyframe;
     - **publisher-side** loss, reordering and RTX repair reach viewers
       correctly (frames compared byte for byte);
     - viewer-side loss is repaired without reaching the sharer;
     - two shares on one constrained viewer pause the lower priority one;
     - stale packets after a drop never cross a segment;
     - unknown and stale-share SSRCs drop;
     - a stopped share frees slots;
     - the keyframe throttle holds under a request flood.
   - **Voice acceptance:** the paired measurements and gate in "Voice headroom
     and acceptance", including the `LATENCY_NET` `rate=` option and the
     screen load generator.
   - CPU per Mbps measured on the VM, recorded here.
4. **Web sharing and watching, plaintext (L, 6–8 days).**
   - `voice/screen.ts`: publish and watch transports, the capture with presets
     and constraints, codec preference, sender parameters with uplink
     headroom, and receiver `jitterBufferTarget`;
   - the stage UI, LIVE badges, sharing bar, fullscreen and PiP.
   - Unit tests for the state machine.
   - Playwright `e2e/screen.mjs` against a real local server:
     - Chromium sharer with **real tab capture** (`channel: "chromium"`,
       `--auto-select-tab-capture-source-by-title`, which spike S4 showed
       works headless);
     - WebKit sharer with a canvas track injected through the test hook;
     - viewers in both engines check decoded frames and a stamped barcode
       (the S3 technique);
     - late join, start → stop → start while watching two other shares,
       moderator stop and the slot limit.
5. **Encrypted shares (L, 8–10 days).**
   - Screen key derivation in `gwar-mls` (agent and wasm), with the
     `"gwar screen v1"` label and vectors.
   - The call controller's per-kind endpoints: attach and restart per kind,
     audio-driven acks, barriers and holds to every endpoint, the single
     activation decision, the screen floor and own-update rule, and the
     worker that outlives transports.
   - The VP9 and VP8 frame layouts, with RFC 9605 test vectors including
     metadata.
   - Keyframe recovery: the receiver gap tracking, `screen.keyframe` retries
     and the sender `generateKeyFrame()` after activation.
   - Fail closed and "Securing screen share…".
   - e2e.md "What the server still sees".
   - AEAD worker cost measured at 1080p60 in all three engines.
   - Tests:
     - controller unit tests: a slow screen install never delays audio acks
       or replaces the audio endpoint;
     - a screen-worker crash commits an own update and never reuses a
       counter;
     - a removal holds both endpoints;
     - Playwright `e2e:screen-mls`: an encrypted share across Chromium and
       WebKit; a member removed mid-share stops decrypting; a joiner watches
       after activation; a viewer installing an epoch late recovers through a
       keyframe.
6. **Desktop enablement (M, 3–4 days).**
   - The desktop UI runs the web screen transports and the screen worker
     beside the native voice engine. The native engine and `voice_set_keys`
     are unaffected.
   - The button is hidden on Linux, and gated to macOS 14+.
   - CSP review: no change expected, since WebRTC is not governed by
     `connect-src`.
   - The native-voice Playwright run (`e2e:native-voice`) extended with an
     encrypted share watched by a browser, with native voice in the same call.
   - The manual checklist from PR 1 repeated on release builds.

**Release acceptance (before the feature is announced)** is a manual matrix:

- sharers and viewers on Chrome, Firefox and Safari, and the desktop on
  macOS and Windows, plaintext and encrypted;
- Firefox both shares and watches. Playwright's Firefox did not start in the
  spike environment, so this interop is checked by hand, or in CI if it runs
  there.

Phase 1 total: about **8–11 weeks**. The earlier 5–7-week figure assumed the
simpler forwarding and key model the review rejected. The estimate stays
uncertain until three things are known:

- PR 1 shows whether the WKWebView picker works without changes;
- PR 3's voice gate passes or forces more isolation (its own thread or port);
- PR 5's controller change lands without reworking the voice activation
  tests.

### Phase 2: quality and reach

7. **Slow viewers without pausing (M, 4–5 days):** VP9 `L1T3` from Chromium
   sharers, and per-viewer temporal-layer dropping driven by the egress
   estimate. Pause stays as the last resort.
8. **Shared audio (M, 4–6 days):** Chromium tab/system audio first, then
   desktop native system audio (macOS ScreenCaptureKit, Windows WASAPI
   loopback). Own playback is excluded.
9. **H.264 with hardware encoding (M, 4–5 days):** NAL-aware SFrame with
   start-code escaping, and the "Game" preset prefers H.264 where the sharer
   has a hardware encoder (`powerEfficientEncoder` in stats).
10. **Pop-out (M, 3–4 days):** a desktop second window, and Document PiP in
    Chromium.
11. **TeamSpeak presence marker (S, 1 day).**
12. **Simulcast (L, 1–2 weeks)**, if temporal layers prove insufficient.

### Phase 3: native and peer-to-peer

- **Native capture and encode in `gwar-client`:** Linux (portal + PipeWire +
  VA-API/software) first, then macOS and Windows for games. str0m frame mode
  with its packetizers; Rust SFrame (5d's library) with the same layouts.
- **Peer-to-peer.** For a share with one viewer, both peers could negotiate a
  direct connection through the server's signalling. ICE, STUN and TURN are
  required; the server already lists `iceServers`, but TURN is not deployed.
  The SFU path stays the fallback and the multi-viewer case. Encrypted shares
  keep SFrame end-to-end on the direct path too, so DTLS alone never decides
  confidentiality. Design this when phase 2 is done.

### Risks and unknowns

- **WKWebView display-capture permissions in a signed Gwar build.** Not
  verified (PR 1). Fallback: native ScreenCaptureKit capture handed to the
  webview is not possible, so the fallback is the phase 3 native path on macOS
  sooner.
- **Background throttling of the webview** while a game is fullscreen (PR 1
  measures it).
- **Encoder CPU on the sharer** for VP9 1080p60 in software on weaker
  laptops. The spike's M5 is not representative. Mitigation: the Game preset
  drops to 720p60 when `qualityLimitationReason` is `cpu`; H.264 hardware in
  phase 2.
- **SFU CPU on the VM.** Extrapolated from an M5 (PR 3 measures).
- **Voice under video load.** Isolation is designed, not yet measured. PR 3's
  paired gate decides whether the shared-runtime task is enough, or whether
  the screen task needs its own thread or port.
- **Controller complexity.** Two endpoint kinds under one MLS call controller
  is the riskiest change to already-reviewed encrypted-voice code (PR 5). It
  needs its own review round, as voice 5c/5d had.
- **WebView2 picker bugs** (#5173 size, #5763 deadlock).
- **Firefox:** no `contentHint`, no `scalabilityMode`, no audio. It works, but
  it is the lowest-quality sharer.
- **Oracle compute allowance** (open question 5).
- **Spike limitations.**
  - One machine, loopback, synthetic content, software encoders and heavy
    background load.
  - No real network and no real display.
  - The SFrame stand-in was a checksum, not AEAD.
  - No Firefox: the Playwright Firefox build did not start in this
    environment, so Firefox rows come from MDN, not from a run. Release
    acceptance covers Firefox interop.

## Spikes

All run on 2026-10-20 on an Apple M5 (10 cores, macOS 27.0 build 26A428),
under load average about 10 from other builds. The code lived in `/tmp` and
was deleted.

**S1: browser capabilities (Playwright 1.63).** Headless Chromium 153 and
WebKit 26.6; Firefox did not launch here.

| | Chromium 153 | WebKit 26.6 |
| --- | --- | --- |
| `getDisplayMedia` | yes | yes |
| `RTCRtpScriptTransform` | **yes** | yes |
| `createEncodedStreams` | yes | no |
| `SFrameTransform` | no | no |
| `CaptureController` | yes | no |
| Send codecs | VP8, H.264 (42001f, 42e01f, 4d001f), AV1, VP9 | H.264 (640c1f, 42e01f), H.265, VP8, VP9, AV1 |
| `scalabilityMode: "L1T3"` | kept | ignored |
| Simulcast offer (3 RIDs) | yes | yes |
| `contentHint = "detail"` | kept | kept |

**S2: system WKWebView as Tauri uses it.**

- A Swift program registered a `tauri` URL-scheme handler, as wry does, and
  loaded `tauri://localhost/`. It used the same public UI delegate as wry
  (grants).
- Results:
  - `isSecureContext` true;
  - `getDisplayMedia`, `getUserMedia`, `RTCPeerConnection` and
    `RTCRtpScriptTransform` all present; no `SFrameTransform`;
  - send codecs H.264, H.265, VP8, VP9, AV1.
- Without a gesture: `InvalidStateError: getDisplayMedia must be called from a
  user gesture handler`.
- With a synthesized click: `InvalidStateError: Document is not fully active
  or does not have focus`. A process launched in the background cannot become
  the active app under macOS's cooperative activation, so the picker step
  remains for PR 1.

**S3: browser → str0m 0.24.1 RTP-mode SFU → browser.**

- A 300-line SFU with ICE-lite on loopback, built the way
  `gwar-server/src/media/webrtc.rs` builds its `Rtc` but with video codecs.
  It forwarded one publisher to N subscribers, rewrote sequence numbers,
  answered NACKs (`nackable`), relayed PLIs and ran egress BWE. It could also
  drop RTP toward subscribers.
- The page drew screen-like content with a 20-bit timestamp barcode into a
  canvas track (`contentHint` detail unless noted), at a 4 Mbps cap.
- An encoded transform emulated SFrame's **byte layout**, without real
  cryptography (layout feasibility only):
  - **whole** = 2-byte header + scrambled frame + 16-byte tag;
  - **clear** = codec prefix clear + scrambled rest + tag.

  Receivers verified a checksum, so "fails" means corrupted frames. 10 s per
  run.
- The S3 SFU renumbered packets in arrival order. That was harmless here only
  because its loss was injected *after* the SFU, toward viewers; S5 shows the
  failure with publisher-side loss.

| Engine, codec, layout | Frames decoded (per viewer) | Notes |
| --- | --- | --- |
| Chromium VP8 plaintext | 271, 271 | p50 57 ms, p95 79 ms; 2.7 Mbps |
| Chromium VP8 whole | **0, 0** | frames decrypt fine (0 fails) but the depacketizer never sees a keyframe: 49 PLIs per viewer, 25 keyframes encoded |
| Chromium VP8 clear (10/3) | 267, 267 | p50 84 ms; 0 fails |
| Chromium VP9 plaintext | 275, 275 | p50 60 ms; 2.0 Mbps |
| Chromium VP9 whole | 272, 272 | p50 74 ms; 0 fails |
| Chromium H.264 plaintext | 247, 248 | p50 108 ms; 1.2 Mbps; software encode 15 ms/frame |
| Chromium H.264 whole | **0, 0** | the packetizer sent **no RTP at all** |
| Chromium H.264 clear (to first slice NAL + 1) | 204, 204 | p50 73 ms; no start-code escaping (see the codec section) |
| Chromium AV1 plaintext | 297, 296 | p50 34 ms; 1.9 Mbps; encode 2.3 ms |
| Chromium AV1 whole | **0, 0** | the packetizer sent no RTP |
| Chromium VP8 clear, 5 % egress loss | 324, 323 | 183/160 NACKs answered by the SFU, publisher NACKs 0, one 201 ms freeze |
| Chromium VP9 whole, 5 % egress loss | 335, 335 | 134/131 NACKs, publisher NACKs 0, one 216 ms freeze |
| Chromium VP9 whole, late joiner (at 6 s) | 342, 173 | joiner decoded after one relayed PLI |
| Chromium VP9 whole 1080p30, 6 Mbps cap | 338, 338 | 4.8 Mbps; p50 57 ms |
| Chromium VP9 whole 1080p60 motion, 8 Mbps cap | 715, 715 | 6.1 Mbps; 60 fps; largest frame 204 KB |
| Chromium VP9 whole, 6 viewers | 335–336 each | SFU CPU 3.3 % of one core for 12.2 Mbps egress |
| WebKit VP8 plaintext / VP8 clear / VP9 whole / H.264 clear | 269 / 266 / 205 / 272 | p50 25–53 ms; 0 fails |

All runs used one Chromium or WebKit instance as both publisher and viewers.
Cross-engine runs (Chromium sharer, WebKit viewer) were not run; PR 4's e2e
covers them.

**S4: real `getDisplayMedia` in automated tests.**

- Headless Chromium (`chrome-headless-shell`) answers `NotSupportedError`.
- The full Chromium build in headless mode (`channel: "chromium"`) with
  `--auto-select-tab-capture-source-by-title=<title>` or
  `--auto-accept-this-tab-capture` returned a real tab track: 1280×720 at
  30 fps, `displaySurface: "browser"`, one tab-audio track. `contentHint`
  stuck.
- So the e2e suite can exercise real capture with no OS permission.

**S5: publisher-side loss and reordering (added after review).**

- The same kind of SFU, with impairments on the *publisher's ingress*: 3 %
  RTP loss and 2 % of packets held behind the next one. str0m's NACKs made the
  publisher retransmit over RTX.
- Two forwarding rules were compared:
  - **arrival**: renumber in arrival order;
  - **source**: `out = base + (src - base_src)` on str0m's extended source
    sequence number.
- Chromium 153, 720p30, one viewer, 15 s per run, about 420 frames encoded.

| Codec, layout | Mapping | Frames decoded | Corrupted frames (checksum) | Viewer NACKs | Publisher NACKs / RTX |
| --- | --- | --- | --- | --- | --- |
| VP9 whole | arrival | 2 | 98 of 390 | 0 | 92 / 105 |
| VP9 whole | source | **423** | **0** of 423 | 131 | 72 / 83 |
| VP8 clear | arrival | 7 | 95 of 386 | 0 | 77 / 87 |
| VP8 clear | source | **412** | **0** of 412 | 153 | 84 / 98 |
| VP8 plaintext | source | **413** | n/a | 160 | 96 / 121 |

- With source mapping: no freezes, no PLIs and 0 packets lost at the viewer.
  The viewers' NACKs for gaps the SFU had not yet repaired went unanswered,
  and the SFU's later repair filled them.
- The first version of the spike lost repaired packets. It called
  `handle_input` twice before `poll_output`, and RTP mode keeps a single
  pending packet. Polling after every input fixed it. The rule is in "The
  SFU" above.

## Open questions for the owner

1. **Linux desktop.** Is it acceptable that the Linux desktop app cannot share
   or watch in phase 1 (Linux users use the browser), with native Linux
   sharing in phase 3? Watching could also be native earlier, since decoding
   is simpler than capturing, but it is still a native video pipeline.
2. **Who can watch.** Only people in the channel's voice (as proposed, like
   Discord), or anyone who can see the channel?
3. **Default permission.** Should members be allowed to share by default
   (`share_screen` in the Member role), or only roles the admin picks?
4. **Default limits.** Are 4 shares per channel, 25 viewers per share, an
   8 Mbps cap and a 300 Mbps server budget right for the public server?
5. **Oracle VM billing.** Oracle's Always Free page now gives every tenancy
   2 cores / 12 GB of A1 for free (1,500 OCPU-hours and 9,000 GB-hours a
   month). The VM was made with 4 / 24.
   - Is the tenancy free-only (the excess must be resized away) or paid (the
     excess is billed)?
   - Phase 1 fits in 2 cores either way. Should the VM be resized to stay
     free?
6. **Shared audio.** Is sharing without sound acceptable for the first
   release, with sound (tab and system audio) following in phase 2?
7. **macOS 13 and older.** Is it acceptable that screen sharing needs
   macOS 14+ in the desktop app, while the app itself still runs on 11+?
