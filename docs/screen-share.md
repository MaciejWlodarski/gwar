# Screen sharing (design)

Status: **design, with the owner's decisions of 2026-10-21 folded in**
([Owner decisions](#owner-decisions)). Built so far: the share state and its
protocol (PR 2), the SFU's transports and forwarding (PR 3a), its allocation,
egress cap, budgets and measurements (PR 3b), and sharing and
watching from the browser in plaintext channels (PR 4,
[as built](#web-client-as-built-pr-4)), the same in the desktop app (PR 5,
[as built](#desktop-app-as-built-pr-5)), and TeamSpeak watch links (PR 6,
[as built](#watch-links-as-built-pr-6)). Phase 1
goes through the server's SFU. Peer-to-peer sharing comes later and is only
sketched at the end. The spike code was thrown away; only its results are
kept here ([Spikes](#spikes)).

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
  WebKitGTK ships without WebRTC, so the Linux app has no screen sharing for
  now (owner decision): Linux users share and watch from a browser.
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
  viewers that switch keys late. In encrypted channels only call participants
  can watch.
- **Plaintext channels ship first.** The first release (PRs 1–6) shares in
  plaintext channels only. Encrypted channels refuse sharing until encrypted
  shares land (PR 7) and never fall back to sending a share in the clear.
- **Who can watch.** In plaintext channels, anyone who can see the channel,
  whether in its voice or not. In encrypted channels, only the call's
  participants, because the keys come from the voice MLS group.
- **Shares have no sound in the first release.** Sound is designed and
  comes in phase 2: it is more than a week of work, and its echo-safe default
  (tab audio) exists only in Chrome and Edge
  ([Shared audio](#shared-audio-phase-2)).
- **Phase 1 is single-layer.** The sharer adapts only to its own uplink.
  - Each viewer's estimate is split across its watched shares in priority
    order (the stage first).
  - Shares that do not fit are paused for that viewer and never slow the
    sharer down.
  - Server resources are reserved by bitrate *cap* and enforced continuously.

  Temporal layers, then simulcast, come in phase 2.
- **Permission:** a new channel permission `share_screen`, which needs
  `connect`. Every member has it by default; admins can remove it.
  Moderators with `mute_members` can stop someone's share. Limits are per
  channel, per share and per server.
- **TeamSpeak users watch through a link.** TeamSpeak has no video. In a
  bridged plaintext channel, the bridge posts a watch link into the TeamSpeak
  channel chat, like attachment links. The link opens a watch-only web page
  with no account, through a capability token scoped to that one share
  ([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6)).

## Goals and non-goals

Goals:

- Share a screen, a window or (in Chromium) a tab into one's voice channel.
- Everyone who can see the channel can open the share (in encrypted channels,
  everyone in its call). TeamSpeak users in a bridged plaintext channel can
  open it through a watch link. A share costs bandwidth only for people who
  open it.
- Low latency, for games and live coding. Target: p50 glass-to-glass at most
  150 ms over the internet at 1080p30, and at most 100 ms within a region.
- End-to-end encrypted in encrypted channels, with the same guarantees and
  limits as voice.
- Works on the deployment VM without transcoding.

Not in phase 1:

- peer-to-peer;
- camera video (the same transport would carry it later);
- recording;
- shared audio (phase 2; [Shared audio](#shared-audio-phase-2));
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
- `audio: false` and `systemAudio: "exclude"` until shared audio lands
  (phase 2);
- a `CaptureController` with `setFocusBehavior("no-focus-change")`, so sharing
  a window does not pull it in front of Gwar.

Other browsers ignore unknown dictionary members, so the same call works
everywhere.

### Desktop app (Tauri 2.12, wry 0.57)

| Platform | Webview capture | Webview WebRTC | Decision |
| --- | --- | --- | --- |
| macOS 14+ | `getDisplayMedia` exposed at `tauri://localhost` (secure context, spike S2) | `RTCPeerConnection` and `RTCRtpScriptTransform` present; encoders VP8, VP9, H.264, H.265, AV1 (S2) | **Webview path** |
| Windows 10/11 | WebView2 = Chromium: `getDisplayMedia` with Chromium's picker; host can veto via `ScreenCaptureStarting` | yes | **Webview path** |
| Linux | portal/PipeWire capture works with wry's `DisplayCapture` permission kind | **no**: `enable-webrtc` defaults off; in 2.52/2.54 the GStreamer backend is incomplete and libwebrtc is not in release tarballs (WPE FAQ; WebKit bug 274711 closed 2026-07) | **Deferred** (owner decision); Linux users share and watch from a browser |

macOS details:

- WebKit routes a display-capture request to its own
  `promptForGetDisplayMedia`, which uses `SCContentSharingPicker` on macOS 14+
  with both windows and displays offered, when the host does not implement
  the private `_webView:requestDisplayCapturePermissionForOrigin:…` delegate.
  wry implements only the public camera/microphone delegate; with no handler
  set, it grants (`wry_web_view_ui_delegate.rs`).
- That holds from WebKit 618.3.11 (Safari 17.6, macOS 14.6). Before it,
  a host that implements the public delegate but not the private one had
  `getDisplayMedia` denied before any picker showed: WebKit bug 274896
  (271688 is the same report), fixed in 279566@main and cherry-picked to
  618.3.6; wry issue #1195. wry 0.57 implements the public delegate, so
  macOS 14.0 to 14.5 cannot share from the app ([PR 5](#desktop-app-as-built-pr-5)).
- The app does not implement the private delegate. On fixed WebKit that
  would make things worse: its two answers, `ScreenPrompt` and `WindowPrompt`,
  limit the picker to displays only or windows only.
- `NSScreenCaptureUsageDescription` is in Info.plist (PR 5), harmless where the
  system picker is the consent.
- The bundle's `minimumSystemVersion` stays 11.0. Below macOS 14.6 the Share
  button says "Screen sharing needs macOS 14.6 or later".

Windows details:

- The picker fails when the webview is under 600 px tall (WebView2Feedback
  #5173). The main window's `minHeight` is already 600.
- There is an open picker-close deadlock (#5763), to be watched.
- wry maps no screen-capture permission on WebView2. The default, which is
  Chromium's picker, is what we want.

#### Capture check (PR 1)

A debug desktop app whose web bundle was built with `GWAR_CAPTURE_CHECK=1`
(for example `GWAR_CAPTURE_CHECK=1 pnpm --filter desktop tauri dev`), and that
is started with it, shows a check page instead of the app. It probes the webview microphone and camera (both must be
denied), calls `getDisplayMedia` on a click, loops the track through a local
`RTCPeerConnection` pair and logs the capture and encoder frame rate, split by
window visibility, every 30 seconds. "Copy report" copies the samples as JSON.
Release builds never show it, and bundles built without the variable do not
contain it (the build fails if they do).

| Platform | Picker | Capture | TCC prompt | fps, window hidden 5 min | Audio offered |
| --- | --- | --- | --- | --- | --- |
| macOS 27.0 (26A428) | not run | not run | not run | not run | n/a |

On macOS 27.0 (26A428), without a human at the screen: `getDisplayMedia`,
`RTCPeerConnection` and `RTCRtpScriptTransform` are present in a debug build;
`CaptureController` is not, so WebKit cannot keep focus on the app when a
share starts. The rest of the table is filled in by hand.

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
- **No shared audio from the desktop app**, even in phase 2's browser audio.
  WKWebView cannot capture audio, and on Windows the system mix would contain
  Gwar's native voice playback ([Shared audio](#shared-audio-phase-2)). Native
  capture that leaves out Gwar's own process comes later (PR 10).

Phase 3 may bring native capture: macOS and Windows for games if measurements
show the webview's encoder or throttling falls short. A native Linux path
(portal and PipeWire capture, libvpx, str0m frame mode) is the one way to give
the Linux app sharing; the owner deferred it.

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
| SFU | < 1 ms for delta frames when the viewer has room; a keyframe leaves over size ÷ (0.8 × the viewer's estimate), e.g. 100 ms for 200 KB at 20 Mbps; no original packet leaves later than 400 ms after arrival; repairs are best effort, bounded in volume, not age ([Queues and repairs](#queues-and-repairs)) |
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
  by `screen.watch.offer` on the first watch and kept while the session is
  connected. It belongs to the session, not to a call, because plaintext
  shares can be watched from outside their voice. Its offer is fixed:
  `SCREEN_SLOTS = 4` `recvonly` video m-lines, nothing to send. Any other offer
  (another count, a slot that may send, audio, a data channel) is refused
  before negotiation, and one that would leave a stream to receive on is
  refused after it (the SSRCs a viewer lists are its RTCP senders). Later, the
  screen task drops any RTP a viewer sends before str0m sees it (str0m would
  set up a stream for it by MID and RID); STUN, DTLS and RTCP pass. Media into
  a watch transport would bypass the publisher's admission and meter. Phase 2
  widens each slot with an audio m-line
  ([Shared audio](#shared-audio-phase-2)). This mirrors the eight voice
  slots: no renegotiation when shares start, stop or change hands. A `run`
  number restarts it, like voice's run.
- **Guest watch transport.** The same, with one slot, for a TeamSpeak guest
  ([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6)).
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
  - **STUN always goes by credentials, voice first** (RFC 7983: first byte
    0–3), even from a screen address: by the local ICE ufrag its `USERNAME`
    names, then `accepts` (whose integrity check covers any address but the
    transport's nominated one). Never by address alone: str0m's `accepts`
    takes anything from a transport's nominated address before it looks at
    credentials, so a stale address would keep another transport's checks.
    A voice peer that takes a check takes the address back from the screen
    set (`disown`), so a NAT that reuses an old screen address for a voice
    client cannot divert that client's voice for longer than one consent
    check. In the screen task, a check claims its address for the transport
    it names.
  - Other datagrams from addresses no STUN claimed go to voice peers'
    `accepts`; the screen task drops them. This is **routing, not
    authentication**: SRTP authentication in the owning `Rtc` is what rejects
    forged media, as for voice today.
  - A screen transport holds at most 4 addresses; a fifth replaces the one
    whose checks stopped longest ago.
- **The hand-off never blocks.** A full channel drops the datagram and counts
  `screen_dispatch_dropped`. Video recovers through NACK/PLI; voice never
  waits.
- **Egress belongs to the screen task.** It sends through a clone of the
  socket handle (`Arc<UdpSocket>`, `try_send_to`, dropping under socket
  pressure like voice). Fan-out, retransmissions and padding probes run there,
  on its own Tokio task. Phase 1 uses the same runtime. If PR 3b's measurement
  shows voice jitter, PR 3b moves it to its own thread.
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
- **PR 3b acceptance compares voice with and without video,** paired runs, using
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
    underruns within baseline + 0.5 %. Missing the gate blocks PR 3b until the
    isolation is changed (own thread, separate port or a lower video
    allocation).

**VM gate follow-up (`07e2f24`).** The supplied samples show viewer 0
stopping near 15 s while the other four sustain 8.54 Mbps. That run logged
only aggregate pause counts, so its exact allocator trigger is unavailable.
The example uses a fresh server with a 300 Mbps default budget; budget
allocation would select the newest watch (viewer 4). The observed pause of
viewer 0 is therefore inconsistent with that path.
A local replay with 8 ms process stalls during startup produced permanent
bandwidth pauses with zero original socket-cap drops and zero extras
escalations. One pause occurred before the 5 s average guard could expire,
identifying the two-keyframe failure path. Separately, the unconstrained
transport simulation sent zero originals and paused at 10.546 s, even
without CPU contention. This is a server pacing/bootstrap defect; increasing
harness worker counts does not repair it. The stalled replay is diagnostic,
not a voice acceptance measurement.

The fixes drain bounded unpaced/probe bursts without per-packet timer wakes,
account for elapsed time on late probe wakes, and update the watch clock
before polling. The probe target now respects the refilled extras reserve.
Screen-watch transports suspend ordinary background padding at its source
while the reserve refills; probe clusters remain enabled. Otherwise str0m's
50 kbps padding can drain the extras bucket faster than its 10 kbps refill
at a 100 kbps estimate,
permanently blocking target growth even after the link recovers. The padding
limit survives estimator updates and resets; background padding returns
when the reserve is ready. Burst sizes are unchanged.
Regressions use an independent viewer clock with 1, 2, 5 and 10 ms SFU
cadences plus periodic 10 ms stalls, require steady video throughput, restore
a link after an actual bandwidth pause, and pin resume at exactly 10 s of
continuous room. Pause thresholds and socket caps are unchanged.

**Local follow-up verification.** All builds used `CARGO_BUILD_JOBS=4`
and the worktree's `target/`. Server unit tests: 287 passed (including 51
screen tests and monthly egress); screen integration: 10; latency example:
27; vendored str0m: 759 unit tests and 14 probe integration tests, including
padding disable/reset/re-enable; Python harness: 9. The no-default-features
server check, clippy with `-D warnings` for the server/proto/client and vendor
library/probe regression, rustfmt and `git diff --check` passed.

The broader vendored str0m suite hits an existing `simple::bwe_cellular`
failure: 403.346 kbps measured against a 750 kbps intermediate threshold.
A scratch export of the unchanged `89dacfe` head, with the same local packet
fixtures, reproduces that identical failure. This fix changes no str0m
production code.

The earlier macOS loopback gate at `a4c76a1` used one pair of 40 markers
per scenario:
unconstrained, all five viewers at 7.87 Mbps with zero pauses,
voice p95/p99 deltas +0.60/+0.25 ms;
downlink-loss, 3.08 Mbps still active, p99 delta
+14.92 ms. Both passed with zero underruns in baseline
and screen runs. The final startup-stall replay kept every viewer at
7.81 Mbps with zero pauses despite 13.78 ms maximum
receive wake lateness; the original replay paused two viewers permanently.
These shortened local runs do not replace the full ten-pair VM gate or its
CPU scaling measurement. No deployment service or VM was contacted.

**As built in PR 3b** (docs/latency.md, "Screen load"; `scripts/latency/screen_gate.py`):

- The gate compares the **median of the per-pair deltas** (screen − baseline)
  and fails if more than **one of ten pairs** exceeds any applicable per-pair
  latency/underrun threshold. Shortened runs allow `floor(pairs / 10)` such
  pairs, so a median cannot hide intermittent starvation.
  "No extra underruns" is the screen runs' total against the baselines';
  the constrained underrun figure is underrun samples over samples played,
  in percentage points.
- Every screen run must report the expected load spec and every expected viewer
  must receive at least 60 % of the scenario's intended steady media rate
  (8 Mbps for unconstrained/uplink; one 3 Mbps share for downlink), over at
  least one settled second. A missing, failed or quiet load fails the run.
- Voice sender/receiver have explicit relay roles: both plaintext and encrypted
  voice rewrite their SDP answer candidates to the role's UDP endpoint. Shared
  seeds select the same voice impairment streams even when screen flows arrive
  first. The relay reports the seeds actually installed on each flow; a real
  voice/relay/screen integration regression pins them. The shared bottleneck
  still intentionally couples queueing across flows.
- The gate output reports `queue_max_ms`, `scheduling_lateness_max_ms` (actual
  relay send minus scheduled release) and `scheduling_late_over_5ms`, separately
  for each direction and run. Queue residence excludes relay/runtime lateness.
- Every viewer also reports `receive_scheduling` in the final JSON and each
  500 ms sample: datagrams, maximum wake lateness, wakes over 5 ms, maximum
  receive/str0m processing time and control-send errors. A 5 ms heartbeat
  measures idle/paused viewers too. These are local processing times, not
  kernel packet-arrival timestamps. `transitions` records the viewer, share,
  reason and wall-clock time of every pause/resume. The gate enables screen
  debug logs by default (unless `RUST_LOG` is supplied); bandwidth pauses name
  their trigger: `average`, `keyframes`, `extras` or `priority`.
- `rate=` is one bottleneck per direction shared by every flow, so the voice
  sender shares the 10 Mbps uplink with the publisher, and the voice receiver
  the 5 Mbps downlink with the viewer. Loss variants add 1 % loss both ways.
  The unconstrained scenarios have five viewers, directly (no loss) or all
  through the relay (loss); the uplink ones one viewer, directly.
- **Deviation:** the downlink scenario uses two **3 Mbps** Smooth shares, not
  4 Mbps. `B` is 80 % of an estimate that stays below the 5 Mbps link
  (4.4–4.7 Mbps measured), so a 4 Mbps share never fits: both paused and no
  video ran beside the voice. With 3 Mbps one share pauses and the other
  runs at 3.0 Mbps.
- The load runs on its own runtime and the relay on its own thread: on the
  example's shared two-thread runtime, the relay forwarding 45 Mbps added
  30–90 ms to the voice beside it, which is the harness and not the network.
- **Local results** (2026-10-22, Apple silicon laptop with 10 cores, shared
  with other agents' builds, load average 4–7; 60 markers a run after a 15 s
  warm-up). Median per-pair deltas in ms (median / p95 / p99), underruns
  baseline → screen:

  | Scenario | Pairs | Δ median | Δ p95 | Δ p99 | Underruns | Gate |
  | --- | --- | --- | --- | --- | --- | --- |
  | unconstrained, 5 × 7.8 Mbps | 2 | −5.7 | −5.1 | −4.6 | 1 → 0 | pass |
  | unconstrained + 1 % loss | 2 | +1.0 | −6.9 | −2.3 | 0 → 0 | pass |
  | uplink 10 Mbps, 7.7 Mbps share | 2 | +4.9 | +0.2 | −4.4 | 2 → 0 | pass |
  | uplink + 1 % loss | 2 | −9.1 | −10.6 | −13.9 | 3 → 0 | pass |
  | downlink 5 Mbps, 2 × 3 Mbps | 2 | +7.1 | −12.0 | −12.0 | 0 → 3 | pass |
  | downlink + 1 % loss | 2 | +50.3 | +55.6 | +66.3 | 0 → 9 | **miss** |
  | downlink, rerun | 4 | +18.1 | +23.1 | +23.1 | 0 → 8 | **miss** |
  | downlink + 1 % loss, rerun | 4 | +12.1 | +2.5 | +2.7 | 1 → 1 | pass |

  The baselines alone ranged from 46 to 90 ms median between runs of the
  same build, so two pairs settle nothing; the misses come from **one
  outlier pair each**. In those runs (2 of 12 constrained-downlink screen
  runs) both shares ended paused, the voice receiver got no audio for about
  140 ms around 12 s, and its playout stayed near 170 ms afterwards; the
  relay's queue never exceeded 31 ms, so the gap was not on the simulated
  link. Eight further single runs with tracing, and three with the failing
  pair's seed, did not reproduce it. Without the outliers the downlink
  scenarios cost voice about +10–20 ms median and a few ms at p95/p99.
- **Left for the VM:** the full ten-pair run, which decides the gate, on a
  host that runs nothing else. If the outlier mode shows there, the next
  step is the isolation the gate names: the screen task on its own thread,
  off the runtime that carries voice. Round 1 adds relay lateness instrumentation;
  it does not diagnose the outlier. Static inspection found no clear, cheap
  blocking operation to remove from the SFU screen loop, so isolation remains
  conditional on the full VM gate.

Round-1 local sanity (debug binaries, one downlink-loss pair with eight markers
per run): the earlier pair passed; the final pair failed p99 (+64.9 ms) and the
new tail rule, with 2.25 Mbps still reaching the viewer. Its downstream queue
max was 15.2 ms and relay scheduling lateness max was 23.5 ms (61 sends over
5 ms). These short runs are not acceptance, and no outlier diagnosis is made.
The separate CPU sanity check observed 4.01 Mbps over a shared 3.51 s interval;
its one point is not a CPU scaling measurement.

### Admission and resource limits

Each share's publish `Rtc` binds its negotiated SSRC and RTX SSRC to an
immutable `ScreenOrigin {session, share, channel, encrypted, private}`. This
is captured at negotiation and re-checked on every forward, like
`AudioOrigin`. Packets of other SSRCs drop.

Limits are enforced continuously, not only when a share or watch is admitted:

- **Per share, ingress.** Two token buckets plus a frame bound, all enforced
  **per packet, on arrival** ("Ingress enforcement" below):
  - bytes: rate 1.25 × the share's cap, burst 512 KB (two 1080p keyframes);
  - packets: rate 2,000 per second, burst 600.

  Payloads up to the negotiated MTU (1200 bytes) are allowed, instead of
  voice's 1033-byte cap. There is no codec parsing beyond the payload
  descriptor's start-of-frame and keyframe bits.
- **Per viewer slot.** A reservation equal to the share's **cap**, not its
  current rate. Static content cannot be admitted cheaply and then grow. Every
  reservation is charged 1.3 × its cap to cover RTX, TWCC padding probes and
  headers. That charge is an admission estimate; the per-viewer egress cap
  in "Queues and repairs" enforces the bytes actually sent. Guests reserve
  the same way as members.
- **Server-wide budget** (default 300 Mbps). The sum of all slot reservations
  plus all publish caps, because ingress costs CPU too. A watch or start that
  would exceed it gets `busy`.
  - **Members come before guests at admission.** When a member's watch or
    start would not fit, the server first reclaims guest reservations,
    newest guest first, across all shares, until it fits. Those guests are
    paused (`screen.paused {reason: "budget"}`) and their reservations are
    released at once, so the member is admitted in the same request. Only
    when no guest reservation is left does the member get `busy`.
  - **A paused guest resumes only by passing admission again.** Every 5 s
    the server re-admits paused guests, oldest pause first, while their
    reservation fits beside every member's. A member is never paused to
    make room for a guest.
  - Without this, four 8 Mbps publishers and 25 guests spread over them
    reserve 32 + 25 × 10.4 = 292 Mbps and lock members out, while real guest
    egress (around 200 Mbps) never trips the measured overrun below.
  - The screen task also measures real egress every second, including
    retransmissions and padding. As built it counts the bytes of every
    datagram its transports hand to the socket (the send function that
    enforces the per-viewer cap) rather than str0m's `Event::PeerStats`,
    which reports per-stream RTP and would miss STUN, DTLS and RTCP.
  - When it stays over budget for 5 s, guest watches are paused first, then
    the newest member watches, one a second while it stays over
    (`screen.paused {reason: "budget"}`; guests arrive with PR 6). A paused
    slot keeps its reservation. Budget pauses resume through the usual resume
    rule once egress has stayed at or under 90 % of the budget for 10 s.
- **Negotiation.**
  - At most 4 `screen.*.offer` per session per minute.
  - A guest connection gets exactly one accepted `guest.offer`. A second one
    closes the connection, so renegotiating means reconnecting through the
    gateway's pre-authentication limits
    ([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6)).
  - At most `shares_per_server` publish transports (default 32) and one watch
    transport per session.
- **Dropping leads to a keyframe.** Every drop, whether from an ingress limit,
  the budget, a queue deadline or a pause, marks the affected share or slot
  **waiting for keyframe**. Nothing more is forwarded to it until a keyframe's
  first packet, which the throttled keyframe rule below requests.
  - A frame whose first packets were already forwarded may end incomplete. That
    is allowed: a viewer's jitter buffer never decodes an incomplete frame, and
    the next segment starts at a keyframe.
  - No buffering of whole frames is needed for this.

### Ingress enforcement

Limits apply to each packet as it arrives (option (a) of the review). The SFU
does not assemble frames before forwarding.

- A packet that does not fit the share's byte or packet bucket is dropped at
  once, and so is every later packet of the share, until the next keyframe
  starts.
  - The SFU recognizes a frame by its RTP timestamp. It recognizes a keyframe
    start by the clear payload descriptor: VP9's start-of-frame bit with P = 0,
    or VP8's S bit with partition 0 and the key bit of the clear payload
    header byte.
  - The share's earlier packets of that frame stay forwarded, so the frame is
    incomplete.
  - The share goes to *waiting for keyframe* for **all** its slots, and one
    throttled PLI goes to the publisher.
- **The server bounds frames.** One RTP timestamp may carry at most 512 KB
  (the byte burst), at most 600 packets (the packet burst), and must end, by a
  marker bit or a new timestamp, within 500 ms of its first packet.
  - A packet that breaks any of these is treated exactly like a bucket overrun.
  - So a publisher that sends continuations without ever starting a new frame
    is cut off after 500 ms at most. It cannot postpone the drop.
- **Repeated overruns end the share.** Three overruns of one share within 10 s
  stop it with `screen.stopped {reason: "limit"}`. An honest sharer's frames
  fit far inside these bounds: the largest frame measured was 204 KB, a
  1080p60 keyframe (S3). Without this rule, a keyframe that can never fit
  would loop through drops and requests.
- **Packet-level overshoot is zero.** A packet is either within the buckets
  when it arrives or it is dropped. Nothing waits for a frame boundary.
- **Frame bounds count every packet of the current frame**, in whatever order
  its packets arrive; a late repair of an older frame counts against the
  buckets only. The VP9 descriptor is parsed to its end (RFC 9628 §4.2), and a
  truncated one is not forwarded.
- **The transport is metered too.** Every datagram of a publish transport,
  before str0m sees it (RTX, padding, duplicates, RTCP, STUN included), passes
  a meter of 2,500 packets/s and 1.5× the share's cap in bytes. Over it,
  datagrams drop before str0m, and the first drop of a run counts as an
  overrun, so a publisher cannot exceed its budget through repairs or padding
  that the media buckets never see.

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
  a drop. It always starts at a keyframe's first packet:
  `base_src` is that packet's `src`, and `base_out` is the slot's highest
  `out` + 1 + 64. The jump makes the viewer treat the hole as an old loss, and
  the keyframe makes it irrelevant.
- Packets with `src < resume_src` are stale and dropped, so a repair from
  before a drop never reaches the new segment.
- Timestamps pass through with a per-segment offset, so a slot reused for
  another share starts a fresh timeline. The marker bit is copied, the payload
  type is mapped to the viewer's, and packets are written `nackable(true)`.
- `write_rtp`'s `wallclock` is the packet's arrival time at the SFU, never its
  release time from the Gwar queue, so str0m's sender reports describe the
  sharer's timeline. Phase 2's shared audio relies on this for lip sync.

Viewer NACKs are answered hop by hop from str0m's send buffer. A gap the SFU
itself has not received yet stays open until the SFU's own NACK to the
publisher is repaired (S5: 72–96 publisher NACKs answered). The repair then
fills it at its mapped `out`. S5 decoded 412–423 of about 420 frames with no
freeze.

Retransmissions resend the same SFrame ciphertext under each viewer's own SRTP
keys. The SFU needs no screen keys for this, and nothing about it weakens the
end-to-end layer.

### Queues and repairs

**What is bounded, measured at the SFU's own transmission:**

- **Original packets** leave the SFU at most **400 ms** after the SFU received
  them from the publisher. Later ones are evicted.
- **Pause, unwatch and slot reassignment** stop everything for that slot:
  originals, repairs and padding that reuses its media. Nothing from before
  the stop is sent afterwards.
- **Repairs** (RTX answers to NACKs, and padding probes that reuse cached
  media) have **no hard age bound**. Their age is normally under 400 + 200 ms,
  but str0m cannot enforce that (below).
- **Volume:** each viewer's originals, repairs and probes together stay
  within a per-viewer egress cap, enforced at the socket send ("Per-viewer
  egress cap" below).

Nothing is promised about downstream network queues; the SFU cannot see them.

**Why str0m's pacer cannot be the queue:**

- with BWE enabled, `write_rtp` queues video into str0m's leaky-bucket pacer;
- when the queue grows, the pacer raises its rate to drain within 2 s, rather
  than expiring packets (`pacer/leaky.rs`, `queue_limit`);
- the queue cannot be cleared through the public API;
- `queue_info()` is a cached sample.

**Original packets: a Gwar-owned queue.**

- Every forwarded video `StreamTx` gets `set_unpaced(true)`. The pacer then
  sends those packets on the next `poll_output` without holding them
  (`pacer/leaky.rs`: "Unpaced packets … are sent immediately").
  - TWCC, BWE updates and probe eligibility stay active (`session.rs`).
  - str0m uses this mode for audio by default.
- Packets wait in the screen task instead, in **one queue per viewer**, kept
  per slot.
  - One leaky bucket per viewer releases them at that viewer's whole budget,
    serving slots in strict priority order ("Bandwidth allocation per viewer"
    below).
  - Released packets go to `write_rtp` and leave on the same iteration's poll.
- **Deadline.** Each packet carries `deadline = arrival + 400 ms`. A packet
  still queued at its deadline is evicted, with every later packet of its
  frame, and the slot waits for a keyframe.
- **Bounded admission.** A packet enters a slot's queue only if the viewer's
  queued bytes stay at or under `max(64 KB, 400 ms × release rate)`.
  Otherwise it is dropped like an eviction. The bound is checked per packet,
  so it never overshoots. As built, a packet that would overshoot first takes
  the room of lower-priority slots (their queues drop, lowest first, and wait
  for a keyframe); only what still does not fit drops from its own slot.
- Both kinds of drop immediately request a keyframe for every dropped lane,
  including a lower slot evicted by admission, through the existing viewer
  budget and publisher throttle. Recovery does not wait for publisher traffic.
- Both kinds of drop are recorded per frame. A keyframe that loses any packet
  either way is a **failed keyframe**, which the pause rule counts.
- **Pause, unwatch and drops clear the queue at once.** Only packets released
  to str0m in the current loop iteration can still leave.
- **Measured, not sampled.** The queue's bytes and oldest age come from Gwar's
  own bookkeeping; `queue_info()` is not used for decisions.

**Repairs: str0m's RTX cache.**

Every video `StreamTx` gets `set_rtx_cache(1024, 200 ms, Some(0.15))`. Its age
limit is a target, not a bound:

- eviction runs only when a packet is cached
  (`streams/rtx_cache.rs`, `cache_sent_packet` → `maybe_evict`);
- it scans in sequence order and stops at the first fresh entry
  (`streams/rtx_cache_buf.rs`, `evict`). The design expected a delayed
  publisher repair with a lower sequence number, cached later, to shield
  older entries behind it. PR 3b's test found that str0m 0.24.1 does not
  cache it at all ("Do not cache values preceding evict position" in
  `rtx_cache_buf.rs`, `push`): it shields nothing, and a viewer's NACK for
  it goes unanswered, so that viewer repairs the frame with a keyframe;
- lookup checks no age;
- padding probes reuse cached packets without the NACK limiter
  (`streams/send.rs`, `poll_packet_padding`);
- `rtx_ratio_cap` is a periodically sampled ratio, not a hard cap.

**Why no age bound is needed:**

- **Freshness belongs to originals,** and the Gwar queue enforces it.
- **A stale repair costs bandwidth, not correctness.** It is the same
  ciphertext with the same mapped sequence number. A viewer only NACKs packets
  it is still waiting for. Its jitter buffer discards a packet for a frame it
  already played or skipped, and a duplicate is dropped.
- **Probes carry no fresh information, only bytes.** str0m probes in short
  clusters (about 15 ms each, `bwe/probe/cluster.rs`):
  - when the desired bitrate rises above the estimate, at 1× and 2× desired,
    capped at 2× the estimate;
  - periodically while application-limited, at least 5 s apart, at 2× desired
    even when the estimate already exceeds it (`bwe/probe/control.rs`,
    `maybe_periodic_alr`);
  - when the estimate has stagnated for 15 s with unmet demand.

  str0m 0.24.1 offers no public setting to cap or disable them; only the
  desired bitrate steers them. Their volume is capped by the egress cap below,
  and their rate by keeping the desired bitrate within twice the estimate
  ("Probing for keyframe room").

**Per-viewer egress cap: originals, repairs and probes together.**

Nothing inside str0m bounds repair or probe volume for a viewer:

- unpaced streams also send queued resends at once (`streams/send.rs`,
  `queue_state`);
- the RTX ratio is recalculated every 50 ms, so repeated NACKs can burst
  first;
- padding bypasses it.

So Gwar enforces the bound where it owns the path: **at the socket send.**
Every `Output::Transmit` of a watch transport goes through the screen task's
send function. SRTP leaves the RTP header in the clear, so each datagram is
classified by its header, with no decryption:

- **Originals:** RTP on a slot's main SSRC. They are already paced by the Gwar
  queue at `B = 0.8 × estimate`, including a conservative 64 bytes per packet
  for serialized RTP extensions/alignment and SRTP overhead. Admission and share
  rate/keyframe meters include that same overhead. An originals bucket at the
  socket checks the actual serialized bytes at `B`, with burst
  `max(2400 B, 10 ms × B)`. UDP/IP bytes are outside this socket-byte cap.
- **Extras:** RTP on any other SSRC, that is, the RTX SSRCs (repairs and
  padding) and str0m's probe SSRC. They pass through **one token bucket per
  viewer**:
  - rate `0.1 × estimate`;
  - burst `max(64 KB, 30 ms × 2 × estimate)`, enough for a str0m probe
    cluster at up to twice the estimate. This is the reserved probe capacity.

  Ordinary background padding is suspended on watch transports while credit
  accumulates between probe clusters, including at the lowest estimates.
- **Control:** RTCP (payload types 192–223), STUN and DTLS are small and
  always sent.

A viewer's sustained egress is therefore at most `0.9 × estimate` plus control
traffic, with bursts bounded by the sum of the originals and extras bursts. RFC 4588 §7 asks for exactly
this: originals and retransmissions counted together. Voice runs on a
separate transport and keeps the remaining 10 %.

Pending BWE events are drained before originals leave the Gwar queue. If an
original is still rejected by the socket bucket after release, its lane is
cleared and its stream wiped, its keyframe is counted as failed when applicable,
and recovery enters the existing viewer budget and publisher throttle.

**When the extras bucket is empty,** the datagram is **dropped at the socket**.
That is immediate and needs no str0m change:

- A dropped repair is like a lost one. The viewer NACKs again, or the slot
  recovers through a keyframe.
- A probe cut short can fail to give a valid result, or give one from the
  packets that arrived, so the estimate can grow more slowly. That errs on the
  safe side.
- str0m already gave dropped packets TWCC numbers, so the viewer reports them
  lost. str0m's loss controller then lowers the estimate of an overloaded
  viewer, which is the right direction.

**Small packets.** The pinned str0m 0.24.1 is vendored with one padding
change (`vendor/str0m/GWAR-PATCH.md`): cached payloads smaller than 240 bytes
are not reused for probes; blank padding is sent instead. Tiny deltas remain
available for NACK repair. The extras bucket still charges serialized wire
bytes. A 30 s simulation with a 20 KB keyframe and 16-byte deltas at 5 fps
checks for local drops and pauses; the e2e tests again use 6-byte packets.

The 64 KB floor matters at low estimates: it is 200 ms of a 2.5 Mbps link.
The paired voice runs found that the probe *rate* matters more than this
volume, which the cap on the desired bitrate handles.

**Escalation.** If the extras bucket drops packets in more than 2 s of any 5 s
window, the viewer's lowest-priority slot is paused with reason `bandwidth`.
The pause wipes that slot's cache, as above. This trigger also respects the
10 s ramp. Resuming follows the usual resume rule.

The 1.3 × charge in "Admission and resource limits" stays an admission
estimate. This cap is what enforces it at egress, per viewer. The server-wide
egress check remains a backstop.

**The one hard lever, used where a hard stop matters:**
`StreamTx::set_rtx_cache` **replaces the cache with an empty one**
(`streams/send.rs`: "Dump old cache"). Pause, unwatch and slot reassignment
call it on the slot's stream. After that, neither a late NACK nor a probe can
send anything of the old media on that stream. As built, pause, unwatch and
reassignment all use the local direction switch described under "Requests",
which drops the stream's held packets and queued resends along with the
cache; the pin test covers `set_rtx_cache` for the fallback.

**Options considered and rejected:**

- **Gwar answers NACKs itself** (str0m's RTX cache off). This would need two
  things str0m 0.24.1 does not offer in RTP mode: incoming NACKs reach the
  application only as debug `RawPacket` events, and `write_rtp` writes the
  main SSRC, with no public way to emit RTX-format packets on the RTX SSRC.
  That means a str0m fork or upstream change, so it is not phase 1.
- **A periodic wipe**, every 100 ms for example, would bound repair age to
  the period plus the 400 ms queue deadline. But every wipe also drops fresh
  entries. Any loss whose NACK arrives after the wipe can no longer be
  repaired, so the viewer must wait for a keyframe: a visible freeze, to save
  bandwidth that stale repairs rarely use.
- **A wipe at each keyframe** costs nothing visible, since the viewer skips to
  the keyframe anyway. But screen shares send keyframes only on request, so it
  bounds nothing.
- **Rebuilding the watch transport** on every pause or unwatch also stops
  everything, but it restarts all four slots with a DTLS handshake. The wipe
  does the same with the public API. Rebuilding stays the **mandatory
  fallback** if a str0m upgrade changes `set_rtx_cache`'s replace-on-set
  behavior, which PR 3b pins with a test.
- **Upstream fix.** If str0m gains an age check on lookup or time-ordered
  eviction, repairs get a hard bound at no cost. That is worth proposing
  upstream, but phase 1 does not depend on it.

**Tests in PR 3b**, with str0m viewers behind a constrained link. Every check
timestamps packets **at the SFU's socket send**, against their arrival from
the publisher. Built as `media/screen/sim.rs`: the screen task's watch
transport, egress and allocation code driven on a simulated clock against a
str0m viewer behind a simulated bottleneck (rate, 100 ms queue, loss, 10 ms
delay), so the timings are exact and the runs repeatable. The relay's `rate=`
serves the voice acceptance instead. Results and deviations:

- **A bandwidth collapse** from 10 to 1 Mbps mid-share: no original is sent
  past 400 ms, and the viewer is paused by the allocation rule.
  Built (`a_bandwidth_collapse_sends_no_stale_original_and_pauses_the_viewer`).
- **Pause and unwatch:** after the pausing iteration, nothing of the slot's
  old media is sent. That covers originals, a NACK for a packet sent just
  before the pause, and a forced BWE probe on the same watch transport, which
  sends only blank padding or other active slots' media. Built
  (`pause_and_unwatch_stop_everything_of_the_slot`).
- **A reordered publisher repair:**
  - 102 arrives at 0 ms and is sent at 390 ms;
  - a delayed 101 is sent at 580 ms;
  - traffic continues every 25 ms;
  - the viewer NACKs 102 at 715 ms.

  The test records whether str0m still serves it, so a str0m change is
  noticed. It asserts the viewer's playout is unaffected (no extra freeze,
  frames byte-identical) and a pause still wipes the shielded entry.
  Built (`a_late_nack_after_a_reordered_publisher_repair_and_a_pause`):
  str0m 0.24.1 never cached the late 101, so 102 is not served (the test
  asserts that); every other packet arrives byte-identical, and the pause
  wipes the cache.
- **A NACK storm:** a viewer NACKs the whole cache repeatedly, 50 times a
  second. Its bytes at the socket stay within
  `(0.8 + 0.1) × estimate × t + burst` plus control traffic. Its
  lowest-priority slot is paused by the escalation rule. Other viewers and the
  voice gate in "Voice headroom and acceptance" are unaffected.
  Built (`a_nack_storm_stays_within_the_egress_cap_and_pauses_the_lowest_slot`)
  with deviations: str0m's RTX ratio cap (0.15) answers only part of a storm,
  which then fits the 0.1 reserve unless media runs near `B`, so the storm is
  fresh NACKs for every packet, replayed every 20 ms, on a 6 Mbps link with
  5 % loss and two shares. Replaying TWCC feedback as well collapsed the
  estimate instead. The single-viewer simulation cannot show other viewers;
  they share nothing with it but the screen task.
- **Probes fit the reserve:** after the bandwidth collapse ends, allocation
  and periodic ALR probes go out whole within the extras burst, and the
  capped and uncapped estimates recover within 10 % of each other. The
  separate recovery regression requires resumed media above 60 % of its
  intended rate. Built (`probes_after_a_collapse_fit_the_reserve`), with a
  finding fixed after the `07e2f24` VM gate: the probe pacer waited between
  individual padding packets instead of draining each generated burst. A
  240-byte blank packet per 2 ms wake limited discovery to about 1 Mbps even
  on a healthy link. The local str0m patch now drains each burst immediately
  and spaces only successive bursts. A viewer whose every slot is paused
  for bandwidth for 15 s still restarts its loss-limited estimator from a
  third of its estimate (at least 100 kbps), with bounded initial probes.
  Before every restart, the SFU lowers str0m's desired bitrate to at most
  1.5 times the restart estimate and updates its cached desired bitrate.
  Keeping the old target would bypass the reach limit, drop probes at the
  unchanged extras cap, and repeatedly break the fit timer. This clamp
  applies on every restart, regardless of how long the link was impaired.
  Capacity rediscovery adds delay after the physical link recovers: the
  resume rule requires **10 s of continuous measured room**, starting only
  once probing has rediscovered enough capacity. Retry is no longer
  required to recover a healthy link. Regressions cover an
  8 Mbps/60 fps share with a 200 KB keyframe from startup, scheduler jitter,
  and an actual bandwidth pause followed by restored capacity. The combined
  regression (`game_recovers_after_scheduler_delay_and_bandwidth_collapse`)
  runs Game at 8 Mbps/60 fps with a 200 KB keyframe and a 10 ms SFU cadence:
  a 20 Mbps link collapses to 1 Mbps at 12 s, recovers at 27 s, and the cadence
  returns to 1 ms at 67 s. It runs until 160 s and also covers keeping the
  10 ms cadence throughout. Each run must resume within 45 s of link recovery,
  remain resumed, and deliver at least 90 % of the intended payload rate in
  each of the final three ten-second receiver windows. Both variants resume
  at 56.45 s (29.45 s after link restoration), with no later pause, 8.00 Mbps
  of received video payload and 8.34 Mbps of serialized originals. Total
  extras drops are 62 with the restart target clamp (145 before that fix),
  down from 15,372 in the original combined scenario.

  The parameterized regression
  (`game_recovers_after_different_collapse_durations`) keeps the 10 ms
  cadence and observes 360 s after link restoration. Collapse duration →
  measured resume delay from restoration: **15 s → 29.45 s; 30 s → 44.45 s;
  30.1 s → 44.35 s; 60 s → 29.45 s; 120 s → 29.45 s**. All enforce the same
  **45 s maximum**, no later pause and at least 90 % of the intended rate
  in each final ten-second receiver window; all measure 7,999,680 bps of
  payload and 8,337,600 bps of serialized originals. The 30.1 s case restores
  the link at 42.1 s and ends at 402.1 s. Before the clamp it never resumed
  and dropped 919 extras; now it resumes at 86.45 s and drops 114 extras.
  These are simulated bounds with healthy restored capacity and feedback,
  including capacity rediscovery followed by 10 s of continuous measured
  room; they are not a ten-second deadline from physical restoration.

  The simulated receiver drains str0m after every datagram, preserving its
  single pending RTP event when several packets arrive within one millisecond.
- **Tests pin str0m behavior:** `set_rtx_cache` empties the cache. Built
  (`set_rtx_cache_empties_the_cache`). The vendored str0m regression
  `padding_limit_survives_reset_and_can_be_removed` checks actual RTP output:
  disabling regular padding stops it at the source, the limit survives an
  estimator reset and subsequent updates, probe clusters still run, and
  removing the limit restores regular padding.

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

The watch transport carries up to 4 slots over one TWCC estimate, so the SFU
decides for the viewer as a whole. Averages are used only to decide pauses;
**sending is peak-capable**:

- **Release rate.** The viewer's leaky bucket releases at
  `B = 0.8 × estimate`. Slots are served in strict priority order (phase 2
  puts share audio first, as a class of its own):
  1. the share on the viewer's stage (`screen.focus {share}`);
  2. the other watched shares in watch order.

  A keyframe burst on any slot can use everything the higher-priority slots
  are not using at that moment. A 204 KB keyframe on an otherwise idle 20 Mbps
  viewer leaves in about 100 ms, whatever the share's average rate.
- **Probing for keyframe room.** The SFU sets
  `bwe().set_desired_bitrate(Σ caps × 1.1 + K_max / 400 ms)`, where `K_max` is
  the largest keyframe seen on the watched shares in the last 30 s, and at
  least 200 KB. The estimate can then grow enough for keyframes to meet the
  queue deadline, even for low-average shares. The cost is str0m's probes,
  short clusters at up to 2× desired, which stay inside the viewer's extras
  bucket ("Per-viewer egress cap").

  Removing the final assignment sets the desired bitrate to zero immediately.
  The socket suppresses any pending non-control datagrams while nothing is
  watched, including probe clusters already scheduled by str0m. A retained
  transport sends zero non-control bytes in the 30 s final-unwatch simulation.

  The target is **at most 1.5 × the current estimate**. str0m's largest
  periodic probe runs at twice that target: a 15 ms cluster uses
  45 ms × estimate before headers, within the unchanged 60 ms × estimate
  extras burst. Increases wait until the reserve has refilled to within one
  MTU. Screen-watch transports suspend ordinary background padding through
  `Bwe::set_padding_bitrate_limit(Some(Bitrate::ZERO))`, so it cannot consume
  the refill while the target waits. Once ready, the background-padding limit
  is removed. Probe clusters still supply TWCC feedback during a pause;
  every extra datagram still passes the actual-byte cap. Decreases apply
  immediately. The earlier 2× reach and its measurements predate the
  per-packet timer fix and do not describe the corrected pacer's burst behavior.
- **Ramp.** A slot that starts carrying a share is not paused for its first
  10 s; failed keyframes and averages start counting after that. Added in
  PR 3b: a watch transport's estimate starts at 2.5 Mbps, where `B` is
  2 Mbps and admission 100 KB, so every 1080p share's first keyframe fails.
  Historical loopback runs before the probe-burst fix needed periodic probes
  to overcome initial undershoot; scheduling jitter could prevent that
  bootstrap entirely. The corrected pacer discovers capacity without waiting
  for original media to fit. The ramp still protects normal BWE startup.
  A viewer too slow for a share sees broken video for up to 10 s before its pause.
- **Pause** a slot for that viewer (`screen.paused {share, reason:
  "bandwidth"}`), lowest priority first, when either of these holds:
  - the sum of its 2 s average and the higher-priority slots' averages exceeds
    `B`, for 5 s;
  - two of its keyframes failed within 10 s, that is, lost packets either
    to the deadline or to admission ("Queues and repairs"). That means the
    keyframes, not the average, do not fit. Counting admission matters: at a
    3 Mbps estimate, `B` is 2.4 Mbps and the admission bound 120 KB, so a
    204 KB keyframe is rejected at admission before any packet reaches its
    deadline.
- **Resume** only after 10 s of continuous measured room:
  `B − Σ higher-priority intended slots ≥ max(1.2 × its average, K_last / 400 ms)`.
  The intended set includes active and paused slots in priority order. Focusing
  an already-paused share reclaims room from lower active slots, one change per
  tick. Lower slots keep media/feedback flowing while the fit timer matures,
  then pause on one tick; the focused share resumes on the following tick.
  `K_last` is the share's last keyframe size. Resuming accounts for the
  keyframe it is about to need, so a viewer is not paused again by the very
  keyframe that resumes it. Resuming opens a new segment from a keyframe,
  with `screen.resumed`.
- The viewer sees "Your connection is too slow for this share" on a paused
  card, with a Retry button (throttled as above).

This is deliberately crude: no per-frame prediction, one pacer per viewer, and
two pause triggers that are both measured. It can pause a viewer of a
low-motion share whose link cannot carry a keyframe within 400 ms. For
example, a viewer with about 4 Mbps of estimate cannot watch a 1080p share
whose keyframes are 200 KB. That is the honest limit of a single-layer share;
phase 2's lower-resolution layer is the remedy.

Tests in PR 3b, with a Text-preset share averaging 0.3–0.5 Mbps, forced to
emit 204 KB keyframes every 2 s:

- **Room for keyframes.** An unconstrained viewer and a 20 Mbps viewer: neither
  is paused, no packet is dropped, and each keyframe leaves the SFU within
  `K ÷ B` plus 10 ms.
- **Admission overflow.** A 3 Mbps viewer: the keyframes fail at admission,
  the viewer is paused within two keyframes, and it is not resumed while its
  estimate stays at 3 Mbps.
- **Deadline expiry.** A 6 Mbps viewer whose stage share takes 3 Mbps: the
  204 KB keyframe fits the 240 KB admission bound but gets only about
  1.8 Mbps, so it expires at the deadline. The average trigger stays below
  `B`, and the keyframe trigger pauses the low-priority slot within two
  keyframes. The stage share is unaffected during this forced-keyframe interval;
  startup BWE undershoot/recovery is outside that interval.

All three are built in `media/screen/sim.rs`, with the forced keyframes
starting after the slot's ramp; "within two keyframes" is measured to the
second keyframe's deadline, when it fails. Also built there: two equal
shares on a constrained viewer pause the lower-priority one, the earlier
watched unless `screen.focus` names the other. A viewer's Retry
(`screen.watch` of a share it already watches) resumes a slot paused for
bandwidth at once, through the keyframe throttle; a budget pause waits for
the server's egress to fall. The server's 10 s under-budget guard and the
viewer's 10 s fit timer run concurrently, so a fitting share resumes after
10 s of global room rather than waiting another 10 s.

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
  - At the limits, one full share (25 viewers × 8 Mbps = 200 Mbps, 90 GB an
    hour) uses the 10 TB in **about 110 hours a month**. The whole 300 Mbps
    budget uses it in about 74 hours.
  - Voice, the web app, file downloads and TeamSpeak count toward the same
    allowance.
- **Monthly egress counter (PR 3b), from the network interface.** Counting
  only Gwar's own sockets would miss the web app, file downloads, TeamSpeak
  and everything else, so the counter reads the host's total instead:
  - Once a minute the server reads the transmitted-bytes counter of the
    default-route interface from `/proc/net/dev` (Linux). It adds the delta
    since the last read to a per-calendar-month total persisted in the store.
    A counter lower than the last read (a reboot or a wrap) counts from zero.
    Default-route selection requires destination/mask zero, RTF_UP and no
    RTF_REJECT, lowest metric, falling back to `/proc/net/ipv6_route` for a
    usable IPv6 default.
    Interface changes rebaseline; they never add that interface's lifetime total.
    The complete checkpoint (counter, UTC timestamp, totals, coverage and warning
    level) is one transactional write; memory changes only after it succeeds.
    An interval spanning UTC months rebaselines instead of charging downtime
    traffic to the new month. Such a month, an initial partial month and any
    unavailable interval have `incomplete: true` in `server.egress`; totals are
    observed usage, not a claim of full-month coverage.
  - The total therefore **includes every service on the host and every
    outbound byte of that interface**: TeamSpeak, a reverse proxy, SSH,
    package updates and backups. Traffic Oracle does not bill (for example to
    its own services in the region) is counted too, so the figure errs high.
  - It covers this VM only. Oracle's 10 TB is per tenancy, so other VMs in it
    are not included.
  - In a container, `/proc/net/dev` shows the container's own interface, and
    the settings page labels the figure "this container only". Off Linux the
    counter is unavailable and the page says so.
  - Server settings show the month's total, with the screen task's own
    bytes (members, guests) beside it as a breakdown.
  - Admins get a warning at 80 % and 100 % of `monthly_egress_tb` (default
    10). There is no automatic cut-off by default. An optional
    `monthly_egress_stop` refuses new watches above 100 %.
  - As built (PR 3b): `server.egress` (`manage_server`) answers with the
    month (UTC), the scope (`host`, `container` or `unavailable`), `incomplete`, the total
    and the screen SFU's bytes to members and guests, and the limits. The
    `ScreenLimits` above are server settings, replaced as a whole through
    `server.update`. Admins online get `server.egress_warning` once per
    threshold and month.
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
  - PR 3b re-measures this on the VM with `scripts/latency/screen_cpu.py`:
    one 4 Mbps share and 1–13 viewers against a separate `gwar-server`
    process, CPU from the process's own counters, the slope over forwarded
    Mbps by least squares. CPU and received-byte deltas now use the same explicit
    settled-load snapshot endpoints (CPU interpolated between 500 ms samples).
    The script checks both separately built binaries. The older local figures
    below predate this window correction and must be remeasured. Locally (the same laptop, shared with other
    builds), forwarding 4, 16, 32 and 52 Mbps cost 5.5, 8.8, 13.7 and 18.9 %
    of one core: **about 0.28 % of a core per Mbps** on top of 4.4 % for the
    rest of the server, about twice S3's figure. Not yet run on the VM.
- **Compute allowance.** The account is Always Free (confirmed by the owner,
  2026-10-21). Oracle's Always Free page gives 1,500 A1 OCPU-hours and
  9,000 GB-hours a month, which is 2 OCPUs and 12 GB running continuously.
  2 OCPU is enough for the phase 1 numbers above, and the defaults below
  assume it.

### Limits (server settings, with defaults)

- Concurrent shares per channel: **4**, which equals `SCREEN_SLOTS`, so anyone
  can watch every share at once.
- Shares per server: **32**.
- Viewers per share: **25**, guests included.
- Guests per share: **10** (`guests_per_share`).
- Server-wide screen budget, reservations as above: **300 Mbps**.
- Bitrate cap per share: **8 Mbps**.
- TeamSpeak watch links: **on** (`screen_guest_links`).
- Monthly egress warning: **10 TB** (`monthly_egress_tb`); no cut-off unless
  `monthly_egress_stop` is set.
- `screen.watch` for a channel the session cannot see: `not_found`, so
  hidden channels are not revealed. For an encrypted share, from someone not
  in its call: `forbidden`.

The owner confirmed these defaults (4 shares per channel, 25 viewers, 8 Mbps,
300 Mbps) on 2026-10-21.

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
- **Sender side, best effort only.** Right after a sharer activates a new
  epoch, it calls `generateKeyFrame()` on the worker's
  `RTCRtpScriptTransformer` where that exists.
  - MDN's browser-compat-data lists it as unavailable in Chromium.
  - The spec's request is asynchronous, so the first frames under the new key
    may still be delta frames.
  - It only shortens recovery. **The viewer-side requests above carry the
    guarantee.**

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

They do not measure real AEAD cost in the worker. PR 7 measures that: worker
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
- **TeamSpeak** has no video, so TS clients never receive a share in
  TeamSpeak itself.
  - In a bridged plaintext channel, TS users get a watch link in the channel
    chat ([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6)).
  - The bridge's presence line in the channel description gets
    " · sharing screen" after the sharer's name. The description already
    lists Gwar users (`Presence` in `teamspeak/bridge.rs`).
  - Encrypted channels are unreachable from TS anyway, never get a link, and
    their description stays the fixed notice.
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
- Default: in `MEMBER_DEFAULT` (owner decision): every member may share, and
  admins can remove it per role, member or channel.
- **Watching needs no permission of its own:**
  - **plaintext channel:** the session can see the channel (private channels
    by their member list, as everywhere else). Being in the channel's voice
    is not required. In a password-protected channel the session must also be
    in the channel or hold `bypass_password`, so the password keeps its
    meaning;
  - **encrypted channel:** the session is in the share's call, because the
    keys come from the voice MLS group;
  - **TeamSpeak guests:** the share's watch-link token.

### Requests (`gwar/4`)

Sharing requests are bound to the voice participation,
`{call, participation}`: a share always comes from one's voice channel, and a
sharing request after leaving the call answers `stale`. Watching requests are
bound to the session, and checked against the watching rules in
[Permission](#permission) on every `screen.watch`.

**Watch access is revalidated whenever it can change,** for every watcher,
wherever the watcher is, not only for the channel's occupants:

- **Effective permissions.** Today `enforce_connect` and
  `enforce_connect_everywhere` (`core/overrides.rs`) run after every change
  that can alter effective permissions: role edits and role permission
  changes, channel overrides, member role grants and removals, owner
  transfer and links. They only look at sessions *in* the affected channels.
  PR 2 adds `enforce_watch` beside them, called from the same places with the
  same scope. It re-checks every watch slot whose *share* is in scope:
  visibility, `bypass_password` for password-protected channels, and call
  membership for encrypted ones.
- **Channel state.** Making a channel private or editing its member list,
  setting or changing its password, and switching it to encrypted run the
  same check for that channel.
- **The viewer's own channel.** Every departure or move of the session runs
  `enforce_watch` on all of its slots: joining another channel, leaving
  voice, a moderator or bridge move (`move_admitted`, `move_client`), and
  `channel_leave`. These paths do not call `enforce_connect`, so they get
  their own hook. A viewer watching a password-protected channel's share
  from inside it, without `bypass_password`, loses the slot on leaving. An
  encrypted share's watch ends when its viewer leaves the call.
- **Session state.** Kicks, bans and disconnects end all of a session's
  watches.

When access ends, the slot is released exactly like `screen.unwatch`, in the
screen task's next loop iteration. The core ends the slot's grant first (a
grant also ends with its share), and the screen task checks grants before it
feeds or polls a watch transport, so a NACK arriving before the `Release`
finds an empty RTX cache. Commands that end things are also fenced: a
transport whose share or lease (a watch transport's, ended when the core drops
it) has ended closes within 100 ms even if its command was lost to a full
queue, and an assignment is recorded only once the queue has room for it. The
screen task's own signals to the core (connected, ended) arrive in order: what
the core's full queue does not take waits behind one reservation in that
queue, in line with every other sender, so a busy core cannot keep passing
over them. Waiting signals a later one makes moot leave (a viewer's earlier
runs; a share's `Connected` once it ended), which keeps at most two per share
and per viewer. Its Gwar queue is cleared, and str0m drops everything its stream
holds: packets written but not sent yet, the RTX cache and queued repairs (a
local switch of the m-line's direction to `inactive` and back, never
negotiated). Its reservation is freed. The viewer gets
`screen.slot {share: null}` and `screen.stopped {reason: "access"}`. Nothing
of the share is sent to that viewer after that iteration; repairs and probes
included ([Queues and repairs](#queues-and-repairs)).

| Op | Body | Reply / effect |
| --- | --- | --- |
| `screen.start` | `{call, participation, preset, surface, guest_link}` (`surface`: `screen`/`window`/`tab`/`unknown`, display only; `guest_link`: post a TeamSpeak watch link where one is possible) | `{share}`, a 16-byte random id; checks `share_screen`, limits and budget; refused in encrypted channels until PR 7. Not announced until its publish transport connects; one not answered 35 s after its start, or not connected 35 s after its answer, ends with `transport`, told to the sharer only |
| `screen.publish.offer` | `{share, ssrc, rtx_ssrc, sdp}` | `{sdp}`; the share's own publish transport. Its first connection announces `screen.started` to the channel. One per share, never renegotiated |
| `screen.stop` | `{share}` | ok; event `screen.stopped {share, reason: "stopped"}` |
| `screen.watch.offer` | `{run, sdp}` | `{sdp}`; the session's watch transport (4 `recvonly` slots). `run` restarts it like voice's run; a run is retired for good once offered, so a failed or closed run and every lower one answer `stale` |
| `screen.watch` | `{share}` | `{slot}`; needs a connected watch transport (`unavailable` otherwise); the server assigns a slot, opens a segment at the next keyframe (throttled request), event `screen.slot` |
| `screen.focus` | `{share}` | ok; the share on the viewer's stage gets first claim on its bandwidth |
| `screen.unwatch` | `{share}` | ok; the slot is freed |
| `screen.keyframe` | `{share}` | ok; throttled PLI. Lets a viewer whose decryptor lost sync, for example after a key change, recover without re-watching |
| `screen.stop_other` | `{share}` | needs `mute_members` in the channel and rank above the sharer; event `screen.stopped {reason: "moderator", by}` |
| `screen.link.revoke` | `{share}` | the sharer, or `mute_members` with rank above the sharer; the share's watch link stops working and its guests are disconnected; final for that share |

Events:

- `screen.started {share, client, channel, preset, surface}`;
- `screen.viewers {share, members, guests}`, to the sharer only, and
  `screen.link {share, active}` to the sharer and, once the share is announced, the channel's moderators;
- `screen.stopped {share, reason: stopped|moderator|left|limit|transport|budget|access}`;
- `screen.slot {run, slot, share | null}`, like `voice.slot`, tagged with the
  watch transport's `run`;
- `screen.paused {share, reason: bandwidth}` and `screen.resumed {share}`.

`Welcome` carries `shares: [{share, client, channel, preset, surface}]` for
every channel the session can see, and `screen.started` /
`screen.stopped` go to everyone who can see the channel. In encrypted
channels this is only what the LIVE badge needs anyway: watching still needs
the call. The channel tree shows a "LIVE" badge next to sharers. That uses
one more client field, `Client.sharing: bool`.

Guests speak a separate, smaller protocol on `/ws/watch`
([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6)).

Lifecycle rules:

- A share ends when the sharer stops, leaves or moves channel, disconnects,
  loses `share_screen` (overrides re-evaluate, like `speak`), the screen
  transport fails, or a moderator stops it.
- A share's publish transport closes with it; its SSRCs die with that `Rtc`.
- A new share always gets a new share id and a new publish transport, so no
  frame from an old share can enter a new one.
- Start, stop and start again while watching other shares touches only publish
  transports; the watch transport and its slots are unaffected.
- Watching a plaintext share survives moving between channels and leaving
  voice while the watching rules still hold. It ends when the session can no
  longer see the channel, or when it leaves a password-protected channel
  without `bypass_password`. Watching an encrypted share ends when the
  session leaves that call.

TypeScript types are generated as today (`cargo test -p gwar-proto --features
ts`). The handwritten client validators gain the new schemas (architecture.md,
"Input bounds").

### TeamSpeak watch links (guests, PR 6)

TeamSpeak has no video, so its users cannot watch inside TeamSpeak. When a
share starts in a **bridged plaintext channel**, the bridge posts a link in
that TeamSpeak channel's chat. The link opens a watch-only page in any
browser, without a Gwar account.

**When a link exists.** All of these must hold:

- **The channel is bridged and plaintext.** Encrypted channels are never
  bridged, and the server refuses to create a link for an encrypted channel
  anyway: a second check, so a bug elsewhere cannot leak an encrypted share.
- **The server's `file_access` is `link`.** This is the same decision as for
  attachment links: `teamspeak/bridge.rs` gives TeamSpeak users only file
  names on a members-only server. One setting covers both.
- **The server has a public URL.**
- **The server setting `screen_guest_links` allows it.** It is on by default.
- **The sharer, or a moderator, has not turned the link off.**

**Posting.** The bridge reuses the path of a relayed chat message with an
attachment (`ts_text`, `plain`, `split_for_teamspeak`):

- The sharer's puppet writes into its TeamSpeak channel. Without a puppet,
  ServerQuery posts it, signed `[name]`, and only if TeamSpeak users are in
  that channel.
- The text is `🖥 {name} is sharing their screen. Watch: {url}`, with the
  name passed through `plain`.
- A TeamSpeak user who enters the channel while the share runs gets a private
  link from ServerQuery once per uninterrupted channel membership. Each private
  delivery has a fresh token bound to that user's TeamSpeak UID and membership;
  it never carries the channel post's shared token. A newly minted token is
  inactive until Query processes the final private-message acknowledgement with
  the original membership still valid. Leaving, moving or disconnecting revokes
  it and terminates guests already using it, including when the same UID later
  returns. A return gets one fresh private link. Confirmed delivery is recorded
  at acknowledgement and retained for its membership, including across a delayed
  receipt sweep.
- Nothing is posted when the share ends. The link simply stops working.
- The channel description's presence line gets " · sharing screen" after the
  sharer's name. The link is not put there: descriptions are visible to
  everyone on the TeamSpeak server, while channel chat reaches only that
  channel.

**The link and its token.**

- The token is 32 random bytes from the OS generator, in base64url.
- The URL is `{public_url}/watch#{token}` when the server serves the web app
  itself (`--web-root`). Otherwise it is
  `https://gwar.app/watch?server={endpoint}#{token}`, the same `server=` form
  that invite links use.
- The token is in the URL fragment. Browsers never send the fragment to the
  web host or in a Referer header. Only the watch page reads it, and sends it
  over the WebSocket to the Gwar server.
- **Where it is kept:**
  - The guest gateway checks SHA-256(token) → share in memory. Private token
    hashes additionally carry the intended UID and an immutable membership
    lease and acknowledgement state. At `guest.watch`, the final private-message
    acknowledgement must have been processed and that UID's original membership
    in the share's channel must still be live; otherwise the reply is the same
    `not_found` as for an unknown token. A reconnect cannot revive an old membership.
    An admitted private guest's SFU grant checks this membership lease directly on
    every forwarded packet and before draining queues or handling RTX. Ending the
    membership synchronously removes media authority, before any cancellation task
    runs. Transport/WebSocket closure and admission cleanup follow asynchronously,
    even without another guest request.
  - The bridge keeps the raw shared URL for the channel post. Private URLs
    exist only in their delivery tasks and queued TeamSpeak commands; their
    registry retains hashes, UID bindings and membership leases.
  - Nothing is written to disk or to logs. Token-bearing wrappers redact
    their contents in `Debug`.
- **When it dies.** The token and the bridge's copy are both removed when
  the share ends (stop, leave, moderator stop, transport failure), when the
  link is turned off, and when the bridge detaches. A server restart ends
  every share, and so every link.
- Each share has one shared token for its channel post and at most **64 live
  private tokens**. Expired memberships release token capacity. All tokens
  share the same share/link lease; share end or link revocation invalidates
  every token synchronously. Guests using any of these tokens share the
  existing 10-guest and 25-viewer limits.
- **Query ordering assumption.** TeamSpeak notifications and command responses
  arrive on one ordered Query stream. A departure preceding clid reassignment is
  therefore read before the acknowledgement of a private message executed against
  that reused clid. The connection task invalidates memberships as it consumes
  each notification, then activates private tokens at the final success response.
  It does this before forwarding the result to a delivery task; neither the bridge
  event loop nor its receipt sweep may activate a token. Before that response,
  redemption is a generic miss even if departure bytes are still unread.
- The shared channel token remains a bearer token for the channel audience;
  TeamSpeak retaining it in channel chat history is accepted.
- Private recovery admits at most **four delivery tasks per share**, without
  waiting in the bridge event loop. Later receipt sweeps reconcile readers
  waiting for capacity. Share or membership cancellation interrupts queued
  sends and flood backoff. Each Query send has a 30 s deadline including
  enqueueing and the reply. An in-flight timeout closes the transport and
  its notification stream, allowing bridge supervision to reconnect; queued
  callers fail instead of waiting behind the stalled command.
- The token grants exactly one thing: watching that share. Guests see no
  other name than the sharer's display name, and no channel, chat or other
  shares.

**Guest sessions.**

- **A separate WebSocket endpoint,** `GET /ws/watch`, so the member handshake
  is unchanged. It reuses the gateway's pre-authentication bounds as they are
  (`gateway.rs`, `preauth.rs`):
  - the client address comes from `client_ip`, honouring
    `--client-ip-header`;
  - `PreAuth::connection` runs before the upgrade. It caps connections
    server-wide and per subscriber, bounds the address table, and applies the
    per-subscriber attempt bucket. Guests and members share these limits;
  - `subscriber` groups IPv6 addresses by /64, as for bans and member limits;
  - `PreAuth::hello` rate-limits the first frame, `guest.watch`, in the same
    bucket as member hellos;
  - `HELLO_TIMEOUT` closes a connection that sends no `guest.watch` in time.
    The same deadline applies again between the reply and `guest.offer`;
  - pre-authentication frame size (`MAX_FRAME`), the per-connection frame
    rate limit, `PING_INTERVAL` and `IDLE_TIMEOUT` apply unchanged.
  - **Heartbeat.** `IDLE_TIMEOUT` (60 s) waits for a text frame, and
    WebSocket Ping/Pong frames do not reset it (`next_text`). So guests use
    the member client's application `ping {}` request: the watch page sends
    it every 20 s, as `net/connection.ts` does, and the server answers it.
    A guest that stops pinging is closed after 60 s.
  - The first frame is `guest.watch {token}`. The reply is
    `{share, sharer, preset, ice_servers}`, or one generic `not_found`
    for unknown, inactive, ended and revoked tokens alike.
  - After that, only `guest.offer {sdp}` is accepted, **once per
    connection**, plus `screen.keyframe` and `ping {}`. The offer is a
    watch transport with **one** slot. A second offer closes the
    connection.
  - Server events are `screen.paused`, `screen.resumed` and `screen.stopped`.
    Then the server closes the socket.
  - There is no identity and no MLS, and nothing is stored on the guest's
    device.
- **The server password does not apply.** The token is the capability, as an
  attachment link is on a password-protected server.
- **Limits:**
  - guests count toward the 25 viewers per share and the server budget, with
    the same cap-based reservation as members;
  - at most 10 guests per share (`guests_per_share`), so a leaked link cannot
    crowd out members;
  - per subscriber (an IPv4 address or an IPv6 /64): at most 3 concurrent
    guest sessions, counted inside `PreAuth`'s per-subscriber connection
    limit;
  - guests never keep a member out of the budget: a member's admission
    reclaims guest reservations, and a paused guest must pass admission again
    ([Admission and resource limits](#admission-and-resource-limits)).
- **Revocation** closes every guest session of the share within one loop
  iteration.

**The page.** A second Vite entry, `apps/web/watch.html`:

- one video element, with fullscreen and picture-in-picture (phase 2 adds
  volume and mute with sound);
- status lines: "Connecting…", "Your connection is too slow for this share",
  "The share has ended", "This link was turned off";
- no MLS, no app stores, no local storage or cookies, and
  `Referrer-Policy: no-referrer`;
- Polish or English from `navigator.language`;
- a small bundle (target under 100 KB gzipped);
- it watches in every browser that can watch: Chrome, Firefox and Safari.

**What the sharer sees.**

- Before starting, in a bridged channel, the preset menu says: "TeamSpeak
  users in this channel can watch through a link. Anyone with the link can
  watch." The checkbox "Post a watch link to TeamSpeak" is on by default.
- While sharing, the sharing bar shows "TeamSpeak link on · 2 guests" with
  "Turn off". Turning it off is final for that share.
- Moderators with `mute_members` can turn it off too, with
  `screen.link.revoke {share}`.
- **Ordering, an acceptance check.** The notice and the checkbox appear
  before anything is published. The client sends `screen.start
  {guest_link}` only after the sharer confirms the preset menu. The server
  creates the token only from that request, and the bridge posts only after
  the share is announced. With the checkbox off, no token exists and nothing
  is posted. PR 6's tests check both orders.

**What each side learns.**

- **The TeamSpeak server and its admins:** the link, so they can watch, the
  sharer's name and the start time. TeamSpeak chat logs keep the link, but it
  is dead once the share ends.
- **The Gwar server:** guests' IP addresses, the user agent at the WebSocket
  upgrade and watch times, plus the same media metadata as for any viewer. It
  learns no identity, and nothing new is logged.
- **Gwar members:** the guest count, which the sharer sees.
- **The web host (gwar.app):** that someone opened `/watch`, and the
  `server=` parameter. Never the token.
- **Guests:** the sharer's display name and the video. Nothing about
  the channel or its other members.

**Abuse.** A leaked token is equivalent to a leaked attachment link: anyone
holding it can watch until the share ends or the link is turned off. That is
bounded by:

- the share's lifetime, and revocation;
- 10 guests per share, and the server budget;
- the per-IP limits.

256 random bits make guessing pointless; the attempt limit protects the
server, not the token. The sharer is told, before starting, that anyone with
the link can watch.

**Estimate:**

- server: tokens, `/ws/watch`, the guest transport, limits, revocation and
  `screen.viewers`, 3–4 days;
- bridge: the post, the late-joiner message, the presence marker and tests in
  the existing bridge test harness, 1–2 days;
- the watch page, 2 days;
- the sharer UI and a Playwright guest e2e, 1–2 days.

In total **7–10 working days, about 2 weeks.**

### Shared audio (phase 2)

The owner made sound optional for the first release: in it only if it is
clearly simple and safe, otherwise phase 2. **It goes to phase 2 (PR 9):**

- **It is more than a week.** Even the echo-safe minimum, tab audio in
  Chromium, touches every layer:
  - Opus m-lines on both transports;
  - SFU forwarding with its own bucket and pacer priority;
  - lip sync;
  - per-share volume and mute;
  - tests with real tab audio.

  That is 4–6 working days. With computer sound on Windows it is 6–8.
- **The safe default covers few users.** Tab audio exists only in Chrome and
  Edge. Firefox, Safari and both desktop apps would share without sound
  anyway, and the desktop needs native capture to leave out Gwar's own voice.
- **Computer sound is not simple.** On Windows it depends on
  `restrictOwnAudio`, which MDN still marks experimental, to keep Gwar's
  voice out of the share.

**What phase 1 keeps ready, so sound is added without rework:**

- A share's publish transport is new for every share, so a share with sound
  simply offers one more m-line. Nothing existing is renegotiated.
- The watch transport has a fixed layout restarted by `run`. Phase 2 widens
  each slot to a video and audio pair; clients and server update together.
- The per-viewer pacer already serves classes in priority order, and audio
  becomes the first class.
- Video already uses each packet's arrival time at the SFU as `write_rtp`'s
  `wallclock`, which is what lip sync needs.

The design below is what PR 9 builds.

**What can be captured, and what is offered.** Gwar's own voice playback must
never be captured into a share. Everyone in the call would hear themselves
again, delayed.

| Where | What can be captured | Is Gwar's voice playback excluded? | Offered |
| --- | --- | --- | --- |
| Chrome/Edge, tab, any OS | the tab's audio | yes: Gwar's own tab cannot be picked (`selfBrowserSurface: "exclude"`) | **yes, by default**: the picker's "Also share tab audio" box |
| Chrome/Edge on Windows, screen or window | the whole system mix (`systemAudio`, `windowAudio: "system"`) | only through `restrictOwnAudio` (Chrome 141+), which removes the capturing tab's own audio; Gwar web plays voice in that tab | **only after the user turns on "Share computer sound"**, with a warning, and only where `getSupportedConstraints().restrictOwnAudio` is true |
| Chrome/Edge on macOS and Linux | tab audio only (MDN: "On Linux and macOS, only the audio of a tab can be captured") | as for tabs | as for tabs |
| Firefox, Safari | nothing: no `getDisplayMedia` audio (MDN `audio_capture_support`) | n/a | no; the share has no sound |
| Desktop on macOS (WKWebView) | nothing, as Safari | n/a | no; macOS desktop shares without sound |
| Desktop on Windows (WebView2) | at most the system mix; WebView2's audio support is unverified (PR 1 checks it) | **no**: voice plays natively in the Gwar process, outside the webview, and `restrictOwnAudio` covers only the capturing document | **no**; native capture that leaves out Gwar comes in PR 10 |

Constraints Gwar passes:

- by default: `audio: true` (so the picker can offer tab audio),
  `systemAudio: "exclude"` and `windowAudio: "exclude"`;
- with "Share computer sound" on: `systemAudio: "include"`,
  `windowAudio: "system"` and `restrictOwnAudio: true`;
- after capture, a system-audio track is kept only if
  `track.getSettings().restrictOwnAudio` is true. Otherwise it is stopped,
  and the sharer sees "Computer sound could not leave out Gwar's voice, so it
  is not shared".

Edge removed `windowAudio` in Edge 143 (MDN browser-compat-data, see the
browser table in [native-media.md](native-media.md)). Edge 143+ ignores the
option like any unknown member, so on Edge the `windowAudio` choices above do
not apply, and what a window share offers there is Edge's own default. The
`restrictOwnAudio` check after capture still decides whether any system-audio
track is kept, so Gwar's voice stays out on Edge as well. Edge therefore
needs its own browser check, separate from Chrome's.

The opt-in warning says: "Everyone watching will hear everything your
computer plays, from every app. Gwar's own voice is left out." The setting is
remembered per device and is off by default.

**The sharer's own speakers.** `suppressLocalAudioPlayback` (Chrome 109+,
tab capture) stops the captured tab from playing locally.

- Gwar offers it as "Mute the shared tab for me", **off** by default, so the
  sharer hears what they share.
- The risk: on speakers, the sharer's microphone also picks up the tab, and
  viewers hear it twice (through the share and through voice), unless the
  voice echo canceller removes it.
- PR 9 measures whether Chrome's echo canceller removes another tab's audio.
  If it does not, the default turns on for sharers without headphones (when
  the output device's label says so; otherwise the user is asked once).

**Transport.**

- **Publish:** a second m-line, `sendonly` Opus, when the share has an audio
  track (`screen.start {audio: true}`): stereo, 48 kHz,
  `maxaveragebitrate=128000`, `useinbandfec=1`, no DTX. There is no RFC 6464
  level gate: music must pass.
- **Watch:** each of the 4 slots is a video and audio pair: 4 `recvonly`
  video and 4 `recvonly` audio m-lines, fixed, as before. In the SFU's
  answer, slot k's audio and video share one `msid` stream, so the viewer's
  browser synchronizes them.
- **SFU:**
  - The audio SSRC is bound in the share's `ScreenOrigin`, with its own
    ingress bucket: 192 kbps, burst 16 KB.
  - Packets are forwarded with the same source-sequence mapping. A segment
    opens at slot assignment, on any packet, because Opus has no keyframes.
  - In the viewer's pacer, share audio is served before every video slot, so
    a keyframe never delays it. The same 400 ms deadline applies.
  - Audio is not NACKed (Opus in-band FEC instead), so it has no RTX m-line.
  - Reservation: 0.2 Mbps more per slot with audio.
- **Lip sync:**
  - `write_rtp`'s `wallclock` is each packet's arrival time at the SFU, for
    audio and video alike; str0m documents this as the simple SFU choice. It
    is never the release time from the Gwar queue.
  - str0m's sender reports then put both on one clock, and the viewer
    aligns them.
  - What remains is the difference in uplink delay between the sharer's audio
    and video, normally tens of milliseconds. PR 9 measures it with a stamped
    flash and click.
- **Pausing a slot stops its video only.** The sound continues, and the card
  says "Your connection is too slow for video". Audio stops only if even
  audio does not fit (an estimate under 0.3 Mbps).

**Viewer controls.**

- A volume slider (0–100 %) and a mute button per share, on its card and on
  the stage. They set the media element's volume directly, with no Web Audio
  graph.
- The volume is remembered per sharer in local settings.
- Deafen mutes share audio too.
- Share audio plays on the voice output device (`setSinkId` where supported).

**Encrypted channels.** PR 9 adds SFrame for share audio, with its own label
`"gwar screen audio v1"`, so it has its own key and counter. The voice rules
apply.

**Tests in PR 9:**

- Playwright: a Chromium sharer shares a tab playing a tone (spike S4 showed
  headless tab capture returns a tab-audio track). Viewers in Chromium and
  WebKit detect the tone with an analyser, check mute and volume, and confirm
  the sharer's voice is not in the share.
- System audio on Windows, `restrictOwnAudio` and lip sync are checked by
  hand.

**Estimate:** 2–3 days for the SFU, 3–4 days for the web client, and 1 day
for SFrame and tests: **6–8 working days, about 1.5 weeks.** Desktop sound
from native capture is PR 10.

## 6. Interface

- **Starting.**
  - A "Share screen" button in the voice panel (`ui/VoicePanel.tsx`), next to
    mute and deafen. It is disabled with a tooltip when missing permission,
    unsupported (Linux desktop, macOS < 14, mobile Chrome/Firefox) or in an
    encrypted channel without transforms.
  - Click → the browser/OS picker → a small preset menu: Auto, Text, Smooth,
    Game, Saver. In a bridged channel it has the TeamSpeak link notice and
    checkbox.
  - The preset can change while sharing, with no renegotiation, through
    `setParameters` and `applyConstraints`.
- **While sharing.**
  - A red "You are sharing" bar with Stop (and Chromium's "Switch source"), at
    the top of the window, in the tray menu, and as a red dot on the tray icon
    on desktop.
  - The bar shows who watches ("5 watching") and, when a watch link is
    active, "TeamSpeak link on · 2 guests" with "Turn off".
  - The sharer's own preview is a small thumbnail, off by default, because a
    mirrored screen is confusing.
  - Browsers add their own indicators (Chrome's "Stop sharing" bar, macOS's
    menu-bar indicator).
- **Discovering.**
  - A "LIVE" badge next to the sharer in the channel tree and the voice panel,
    in every channel one can see.
  - Clicking it, or "Watch" on the share card, opens the share. Nothing
    streams before that. In plaintext channels this works without joining
    the channel's voice. In encrypted channels the button says "Join the
    call to watch".
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
  - No sound in the first release; phase 2 adds a volume slider and mute per
    share ([Shared audio](#shared-audio-phase-2)).
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
  in the voice panel's menu. On Linux desktop the LIVE badge shows, and its
  tooltip says that watching needs the browser for now.

Polish and English strings are added together, as for every UI change.

### Web client as built (PR 4)

Code: `apps/web/src/voice/screen.ts` (capture, presets, publish and watch
transports), `state/screen.ts` (the state machine), `state/screen-controller.ts`
(the protocol) and `ui/Screen.tsx` (button, preset menu, sharing bar, LIVE
badge, stage). Decisions taken while building it, where this design left room:

- **The desktop app** shares and watches from PR 5 on
  ([as built](#desktop-app-as-built-pr-5)).
- **Encrypted channels.** The Share button stays visible but disabled, labelled
  "Screen sharing in encrypted channels is coming soon". The client also checks
  before `screen.start` and before watching, so nothing is captured, sent or
  watched there even if the server's refusal were missing.
- **Auto preset without a surface.** Browsers that do not report
  `displaySurface` (and canvas tracks) get Smooth, as screens do.
- **Uplink headroom floor.** The `0.7 × availableOutgoingBitrate` rule never
  lowers the cap below 250 kbps: an estimate that low recovers by probing, and a
  cap under it would starve the probe. It is re-read every 2 s and applied when
  it moves by more than 10 %.
- **Changing the preset while sharing** changes the track constraints and the
  sender parameters only. The server keeps announcing the preset the share
  started with: there is no request to change it, and its per-share limit is
  the server's cap, not the preset's. A change made while `screen.start` or
  the publish negotiation is still running is applied when the publisher
  starts.
- **"Switch source"** is Chromium's own button, enabled by
  `surfaceSwitching: "include"`; Gwar adds none.
- **`screen.stopped {reason: "access"}`** ends only my watch. The share stays
  listed while its sharer's `Client.sharing` is on, because the same reason
  also ends a share whose sharer lost `share_screen`, and only the sharer's
  `sharing` going off tells the two apart. The server frees the slot
  (`screen.slot {share: null}`) before it sends `screen.stopped`; the client
  keeps the freed watch until the reason arrives, so the right notice shows,
  and none for a share it was not watching.
- **Watch transport.** One per session, negotiated on the first watch.
  `screen.watch` is asked again for up to 5 s while the server answers
  `unavailable`, because the browser can see the transport connect before the
  screen task reports it. A transport that fails ends every watch, as the
  server does, with one notice; the user opens the shares again, and the next
  watch negotiates a new run.
- **Watch requests.** Each watch has its own id, and the requests of one share
  run one at a time: a watch, its release and the next watch of the same share
  reach the server in order, so an earlier watch's assignment or release is
  never taken for a later one's. Only the answer to a watch's own
  `screen.watch` makes it a watch (`screen.slot` events only record the slot).
  A request whose watch was stopped, or whose transport was replaced, stops
  before its next step; a slot granted to it anyway is released. Retiring a
  watch transport that never connected (a reset or reconnect mid-negotiation)
  settles whoever waits for it, and a reset starts the queues afresh: an
  earlier session's requests settle without sending anything and hold up
  nothing of the next.
- **`screen.paused` / `screen.resumed`** arrive with PR 3b; the stage does not
  show "Your connection is too slow" yet.
- **The stage** shows the share in focus, then every other watched share and
  every unwatched share of the same channel as cards. Moderators get "Stop
  screen share" on the stage and in the channel tree's menu. On narrow screens
  the stage replaces the chat, with no chat beside it.
- **`guest_link`** was always false until PR 6 added the TeamSpeak checkbox
  ([Watch links as built](#watch-links-as-built-pr-6)).
- **The e2e's WebKit sharer** gets its canvas track from an init script that
  stands in for `getDisplayMedia`; the app's own capture path runs unchanged
  around it. The Chromium sharer captures a real tab (S4's flag). The tab that
  calls `getDisplayMedia` must be in front: from a background tab Chromium
  answers `InvalidStateError`, which the app reports as a capture failure.
- **WebKit on Linux** (Playwright's GTK/WPE build, which CI runs) paints
  remote video unreliably: a `<video>` often stays empty while its receiver
  keeps decoding, mostly when it gets the stream after frames already flow. A
  page with two peer connections and no Gwar code shows the same. Safari on
  macOS paints every video. So on Linux the e2e's WebKit viewers check the
  receiver's decoded frames instead of the barcode; the macOS runs read every
  barcode in both engines, and on Linux Chromium viewers read the WebKit
  sharer's barcode. Every source has a frame of its own shape (16:10, 16:9,
  4:3, 1:1, 3:2, and 1.4:1 for the restarting sharer's second share), and each viewer checks the decoded aspect ratio of the slot
  the app holds for that share: that ties the slot to its sharer even where no
  pixel can be read. Pixels cannot be read there: WebKit has no
  `MediaStreamTrackProcessor`, and `VideoFrame` and `requestVideoFrameCallback`
  need a painted video. Linux WebKit is not a supported platform (see the
  desktop table above).


### Desktop app as built (PR 5)

The desktop app runs PR 4's web code in its webview: the same capture,
publish and watch transports and the same UI, beside the native voice engine.
Video never crosses into Rust. The native engine, `voice_set_keys` and the
voice command contract are unchanged. In plaintext channels there is no
screen worker; it arrives with PR 7's encrypted shares.

- **Where it shares.** A new app-owned command, `screen_capture_support`
  (`src-tauri/src/screen.rs`), answers once at start:
  - `ok` on macOS 14.6 or later and on Windows. On macOS it reads the major
    version from `NSProcessInfo.operatingSystemVersion` and the version of the
    WebKit the app loaded (`CFBundleVersion` of `com.apple.WebKit`; a Safari
    update can replace the system's). Sharing needs macOS 14 and WebKit
    618.3.11 or later (§1, "macOS details");
  - `macos_too_old` otherwise on macOS, and when the WebKit version cannot be
    read: the Share button is shown disabled, "Screen sharing needs macOS 14.6
    or later", and no picker opens; watching works wherever the webview has
    `RTCPeerConnection`;
  - `unsupported` on Linux: no Share button, and the LIVE badge's tooltip
    says watching needs the browser (owner decision 1).
  - Until it answers, the desktop neither shares nor watches; if the command
    fails, it stays that way.
  - The controller checks the same answer before opening the picker, not only
    the button.
- **Windows** runs WebView2's Chromium picker, as §1 says. The app's origin
  there, `http://tauri.localhost`, is a secure context (Chromium treats
  `*.localhost` as potentially trustworthy), so `getDisplayMedia` is exposed.
  `CaptureController` and `surfaceSwitching` work as in Chrome.
- **macOS** runs WebKit's own picker path (§1). WKWebView has no
  `CaptureController` (PR 1), so sharing a window may bring it in front of
  Gwar; the call works without it.
- **A refusal is never silent.** WebKit and Chromium answer both a closed
  picker and a refusal before any picker (a WebKit that denies, a missing
  user gesture, a platform policy) with `NotAllowedError`, and nothing,
  timing included, tells them apart (a busy renderer delays a refusal; a
  quick Escape looks like one). Every `NotAllowedError` therefore shows the
  neutral notice "Screen sharing was cancelled or denied (…)".
- **Info.plist: `NSScreenCaptureUsageDescription`; entitlements: none.**
  - Two consent paths exist on macOS. `SCContentSharingPicker`, which
    WebKit uses on 14+, is consent by itself: the person picks a window or
    display and macOS asks for no Screen Recording permission. Capture without
    the picker (`SCShareableContent`, or WebKit's own window list before 14)
    needs Screen Recording permission in Privacy & Security, and
    ScreenCaptureKit's documentation asks for an
    `NSScreenCaptureUsageDescription` string for that prompt.
  - The app gates sharing to the picker path, so the key is not expected to
    be shown. It is added anyway ("Gwar shares the screen or window you pick
    with your voice channel."): it is harmless, and it gives the prompt a
    reason should a macOS version ask after all. The manual checklist
    records which happens.
  - The app is not sandboxed, and the hardened runtime has no screen-capture
    entitlement.
  - `NSCameraUsageDescription` (wry #1195) stays out. The issue's failure is
    the WebKit refusal above, which the version gate covers, not a missing
    camera string; the manual checklist confirms the picker without it.
- **CSP: no change.**
  - WebRTC is not governed by `connect-src`, and the policy sets no `webrtc`
    directive.
  - A `MediaStream` in `video.srcObject` is not a fetch, so `media-src` does not
    apply.
  - There are no workers in plaintext; PR 7's worker is a bundled script,
    allowed by `default-src 'self'`.
  - `frame-src 'none'` stays.
- **Tray.** The tray menu gets "Stop sharing screen", enabled from the picker
  until the share ends, and the tooltip says "Gwar (sharing your screen)".
  - The red dot on the tray icon (§6) is not drawn. The macOS icon is a
    template image, which the menu bar recolors, so a red dot would need a
    second, non-template icon pair for light and dark menu bars. macOS already
    shows its own screen-recording indicator there.
- **Tests.**
  - Unit tests:
    - the platform gate (Rust);
    - the button, the badge and the controller for each answer (vitest);
    - the tray tooltip.
  - The native-voice Playwright run (`e2e:native-voice`) now ends in a
    plaintext channel:
    - a device driving the native engine shares a canvas through the
      webview's transports, and the browser device watches it from the LIVE
      badge and reads its barcode;
    - voice goes on both ways while it shares, and after it stops.
  - The e2e runs the web build in Playwright's browsers, with a fixture
    driving the native engine; it does not run Tauri. The desktop gate, the
    tray and the real pickers are covered by unit tests and by hand only:
    real capture in WKWebView and WebView2 needs a human at the picker, so
    PR 1's checklist is repeated by hand on release builds.
- **Follow-up.** `NSProcessInfo.beginActivity` while sharing waits for PR 1's
  hidden-window frame-rate measurement (§1, "Tradeoffs").

### Watch links as built (PR 6)

Code: `crates/gwar-server/src/core/screen/guests.rs` (tokens, guests and
revocation), `gateway.rs` (`/ws/watch`), `preauth.rs` (guest limits),
`teamspeak/bridge.rs` (the post, late joiners, the presence marker),
`apps/web/src/watch/` with `watch.html` (the page) and the sharer's checkbox
and link line in `ui/Screen.tsx`. Decisions taken while building it, where the
design left room:

- **The token is made at the announcement.** `screen.start {guest_link}` only
  records the wish. The token is generated when the share is announced (its
  publish transport connected), and only if a link is possible then and the
  sharer has not turned it off meanwhile. With the checkbox off no token ever
  exists. The core keeps SHA-256(token) → share for the shared channel link;
  its raw URL goes once to the bridge, in a redacted `WatchUrl`. Private
  delivery mints distinct tokens in a registry shared by the core and bridge,
  bound to the recipient's TS UID and uninterrupted channel membership. The
  registry keeps them inactive until the ordered Query acknowledgement, checks
  the share lease and membership at redemption, and keeps at most 64 live private
  tokens per share. Every guest's SFU grant retains the link lease; private guests'
  grants also retain the original membership lease. Ending either lease stops fresh
  forwarding synchronously. Queue and RTX cleanup checks the same grant, so pending
  media and repairs cannot outlive that authority. Share end uses the grant's existing
  source authority. Transport/WebSocket teardown and admission cleanup stay async.
- **"Possible" means:** a public URL, `screen_guest_links` on, `file_access`
  `link`, a bridge attached right now (not only `--teamspeak`), and a channel
  that is neither encrypted nor private. Private channels are never bridged,
  so they get no link either. The web client checks the same (`server.teamspeak`
  stands in for the bridge) before it shows the notice and the checkbox, and
  sends `guest_link: false` where the server would make no link anyway.
- **Turning off is final, whoever does it.** `screen.link.revoke` by the sharer
  or a moderator; `file_access` set to `members` or `screen_guest_links` set
  off (checked on every `server.update`, and with the watch rules after any
  channel change); and the bridge detaching or being replaced. Each turns every
  affected link off for the rest of its share: turning a setting back on does
  not bring a link back, the next share gets a new one.
- **What guests are told.** A share that ends sends its guests
  `screen.stopped {share, reason}` without `by`; a link turned off sends
  `screen.link {share, active: false}`. Then their connection closes. The page
  says "The share has ended" or "This link was turned off".
- **Errors.** Unknown, ended and turned-off tokens answer the same
  `not_found` ("this link does not work"). A share already at 10 guests, or at
  25 viewers with members and guests counted together, answers `busy`: telling
  a guest that the link works but the share is full reveals nothing a valid
  token holder could not see anyway. A guest frame that does not parse as one
  of the watch ops, a second `guest.watch` and a second `guest.offer` close the
  connection, even past the rate limit; an error reply echoes the frame's own
  `id` where it has one.
- **A guest let go is gone at once.** The core holds a signal for each guest
  and drops it with the guest; the gateway's writer then stops even a write
  stuck on a guest that no longer reads, flushes the last frames and the close
  within 2 s, and drops the socket. Any frame that cannot leave within 5 s
  ends the connection too. Its pre-authentication place frees with it.
- **The token never reaches a log.** It travels as `WatchToken` from the
  frame to the core and the bridge's copy as `WatchUrl`; both print `(..)` in
  `Debug`. The puppet's queued text (`MessageText`) prints only its length.
- **Guest ids** come from the session id space and are never handed to a
  member, so a guest's watch lease, slot and `EndWatch` cannot collide with a
  session's. A guest has no client entry and appears in no event other than
  the share's guest count.
- **Who sees the link state.** The sharer and moderators (`mute_members`) in
  the share's channel get `screen.link` events while the share runs; `Welcome`
  carries no link state, so a moderator who connects later sees no "Turn off
  TeamSpeak link" until the next change. The sharer's own line comes from its
  `screen.link {active: true}`.
- **The server setting** has a switch in Server settings, shown only on
  TeamSpeak servers.
- **Late joiners and the presence marker.** Private links go once per UID and
  uninterrupted channel membership, using a fresh per-recipient token. Query
  records successful delivery at acknowledgement. Completed receipts are
  consumed before an old attempt is replaced, and a departure never discards
  an acknowledged receipt. That membership's token is revoked on departure;
  returning starts a new membership with one fresh private link. Readers of
  the channel post count as told only after confirmed delivery and while their
  original membership remains valid. If the signed channel post fails, those
  in the channel get private links instead. The " · sharing screen" marker
  follows the sharer's name in the channel description's presence line, which lists users without a
  puppet (the line puppets do not need).
- **ServerQuery posts only to readers.** Without a puppet, or when the
  puppet's queue does not take the post, the signed ServerQuery post is made
  only when TeamSpeak users are in the channel; one who arrives later gets the
  late-joiner message instead. Enqueue is not delivery: the puppet keeps a receipt
  through moves and TeamSpeak's command result. Readers count as told only after
  success. A definite rejection or a command dropped before dispatch uses the
  signed fallback; a lost connection after dispatch leaves delivery unknown and
  does not repeat the channel post. Untold readers who remain in the channel get
  one private recovery attempt per uninterrupted channel membership, with at most
  one flood retry. Each receipt belongs to one post attempt. A departure or move
  invalidates a reader's membership, so leaving and returning before a receipt
  cannot count as reading the channel post. Private delivery runs outside the
  bridge event loop, with four tasks admitted per share and no tasks waiting for
  admission. Query checks the recipient's connection and membership before
  every socket dispatch and retry, invalidating them before forwarding departures.
  Private tokens remain inactive until the connection task reads the final
  successful acknowledgement with the original UID/membership still valid.
  The single ordered Query stream places a departure before the acknowledgement
  of a command executed against a reassigned clid, closing the unread-notification
  redemption race. Redemption and subsequent offers and media callbacks recheck
  the retained membership. The SFU grant checks that same membership synchronously
  on every packet and when clearing queued media and RTX, independently of the
  cancellation waiter. That waiter ends the transport lease before waiting for core
  queue capacity to release the guest admission. A Query disconnect
  also invalidates its memberships and terminates their admitted private guests.
  The core ends the link's cancellation lease synchronously on revocation or share end; all private tokens, queued posts,
  admission deferrals, sends and flood retries obey it. Queue admission and
  reply waiting share a 30 s deadline per send; an in-flight timeout closes
  Query and lets bridge supervision reconnect. A post too long for one
  puppet command goes entirely through ServerQuery, avoiding partial
  enqueue and duplicate fallback posts.
- **The page** is plain TypeScript (no React, no app stores), about 8 KB
  gzipped with its CSS. It keeps the token in the fragment (a reload works
  after a lost connection), pings every 20 s once admitted, and shows
  "Connecting…", "Your connection is too slow for this share" (on
  `screen.paused`, from PR 3b), "The share has ended", "This link was turned
  off", "This link doesn't work: the share has ended or the link was turned
  off.", a busy line and a lost-connection line, in Polish or English. With
  `--web-root` the server serves it at `/watch` and `/watch.html` with
  `Referrer-Policy: no-referrer`, `Cache-Control: no-cache`,
  `Content-Security-Policy: frame-ancestors 'none'` and `X-Frame-Options:
  DENY`. The official web host serves it the same way and proxies
  `/ws/watch` (docs/deploy.md, nginx).
- **Tests.** `crates/gwar-server/tests/watch_links.rs` runs guests over real
  SRTP against a channel standing in for the bridge, including the PreAuth
  limits by /64 and the 60 s idle close of a guest that stops pinging. The
  bridge tests cover confirmed puppet delivery, rejection, a dropped queued
  post, uncertain delivery without duplicate fallback, stale receipts and
  cancellation during a query move. Query tests cover cancellation in the
  socket queue and before a flood retry. A deterministic single-thread regression
  writes departure/reassignment and acknowledgement bytes before redemption,
  attempts redemption before Query can consume them, and verifies the generic
  miss both then and after the acknowledgement. It also covers the normal ordered
  acknowledgement and activation path. Guest gateway tests negotiate real SRTP,
  receive 12 packets, end the membership, and immediately publish more media before
  awaiting socket closure; the revoked guest receives zero RTP. They also verify
  socket closure and freed admission. A deterministic core/SRTP regression uses the
  actual guest grant without yielding to cancellation tasks: membership, link and
  share revocation reject fresh writes and clear queued packets and delayed RTX,
  while the transport lease and admission still exist. Core tests fence queued offers and delayed connection
  signals after membership ends; verify live, departed, returning and ended token
  bindings and the 64-token cap; exercise both receipt-sweep orderings with one
  delivery per membership; and keep 40-recipient recovery bounded with a stalled reply, share-end
  cancellation and Query reconnection signaling. They also cover the signed
  ServerQuery post, late joiners, encrypted channels, members-only servers
  and the description.
  `apps/web/e2e/watch.mjs` runs Chromium and WebKit guest pages against a
  server built with `test-hooks`, whose stand-in bridge
  (`GWAR_TEST_WATCH_LINKS`) prints each link: the acceptance order, watching,
  the 150 s heartbeat, the end of the share, the checkbox off and "Turn off".
- **Waiting for PR 3b** (allocation): a member's admission does not yet
  reclaim guest reservations (newest guest first; each guest records when it
  joined for this), `screen.paused` / `screen.resumed` are not yet sent to
  guests, and a paused guest is not yet re-admitted every 5 s. Until then a
  guest holds its viewer place like a member, within the 10-guest cap. The
  page already handles both events.

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
     five minutes) and Windows 10/11 (picker; frame rate; whether WebView2
     offers any audio with `audio: true`, recorded for later).
   - Results are recorded in this document.
   - This settles the only unverified premise of the webview path before
     anything depends on it.
2. **Protocol, permission and share state (M, 4–5 days).**
   - `share_screen`, in `MEMBER_DEFAULT`;
   - the requests and events above, except the `*.offer` and slot ones,
     including `guest_link`, `screen.viewers` and `screen.link`;
   - the watching rules: session-bound watching, channel visibility, the
     password rule and call membership for encrypted channels;
   - `enforce_watch` at every `enforce_connect` call site, on every departure
     or move of the viewer (`move_admitted`, `move_client`, `channel_leave`),
     and on the channel and session changes listed under "Requests". Tests:
     - revoking `bypass_password` by role edit, override and role removal
       from a viewer outside the channel;
     - a viewer without it leaving, switching channel, and being moved out
       by a moderator, each ending the watch of a password-protected
       channel's share;
   - shares listed for every visible channel, without revealing hidden ones;
   - limits, lifecycle and the moderator stop;
   - `screen.start` refused in encrypted channels until PR 7;
   - `Client.sharing`;
   - shared role-resolution vectors;
   - server tests in `tests/protocol.rs`, `tests/overrides.rs` and
     `tests/moderation.rs`;
   - the client validators;
   - no media yet.
3. **SFU video forwarding (L, 8–12 days), in two PRs.** Forwarding and
   allocation are each large enough to review on their own, so PR 3 is split.
   PR 3a carries media end to end; PR 3b makes it fair under constrained
   links and measures it. Nothing ships to users before PR 4, so 3a's gaps
   are never live.

   **PR 3a: transports and forwarding.**
   - The address-map dispatch and bounded hand-off from the voice task, and
     the screen task.
   - `screen.publish.offer` and `screen.watch.offer`, and their `Rtc`s
     (VP9/VP8 + RTX), polling after every input. A share is announced when
     its publish transport connects, and ends with `transport` if it has not
     35 s after the answer.
   - Share admission: the `ScreenOrigin` binding and byte/packet buckets.
   - Slot assignment, `screen.watch`/`unwatch`/`focus`/`keyframe` and
     `screen.slot`. `screen.focus` is recorded; it takes effect with 3b's
     allocation.
   - Stable sequence mapping with segments, and timestamp offsets.
   - Per-packet ingress enforcement with frame bounds; wait-for-keyframe;
     the one keyframe throttle, and NACK/RTX with the RTX cache wiped on
     unwatch and reassignment. Forwarding writes straight to str0m's unpaced
     streams.
   - Counters in `ScreenCmd::Inspect`.
   - Tests (`tests/screen.rs`, with str0m clients as sharer and viewers):
     - a late viewer starts at a keyframe and receives every byte in order;
     - publisher-side loss is repaired into place; viewer-side loss is
       repaired without reaching the sharer;
     - unwatching, stopping and losing access end the media at once;
     - a bucket emptying mid-frame cuts until the next keyframe, and three
       overruns end the share with `limit`;
     - the keyframe throttle holds under a request flood;
     - transports are negotiated strictly (one video m-line with its RTX
       group, idempotent offers, `stale`/`conflict` runs).

   **PR 3b: allocation, egress and measurement.**
   - The Gwar-owned per-viewer priority queue with 400 ms deadlines and
     bounded admission; the RTX cache also wiped on pause; the per-viewer
     egress cap at socket send.
   - Per-viewer aggregate allocation with pause/resume, honouring
     `screen.focus`.
   - Cap-based reservations, continuous egress measurement and negotiation
     limits.
   - The monthly egress counter and admin warnings.
   - The remaining tests:
     - two shares on one constrained viewer pause the lower priority one;
     - a viewer outside the share's voice watches a plaintext share, and
       loses it when the channel becomes hidden to it;
     - a viewer outside a password-protected channel, watching through
       `bypass_password`, loses the slot when a role edit, an override or a
       role removal takes that permission away. Nothing of the share reaches
       it afterwards, NACK answers and probes included;
     - a viewer without `bypass_password` watching from inside a
       password-protected channel loses the slot when it leaves voice, joins
       another channel, or is moved out by a moderator or the bridge, with
       the same cleanup;
     - the tests in "Queues and repairs": a bandwidth collapse,
       pause/unwatch (NACK and probe included), a reordered publisher
       repair, a NACK storm against the per-viewer egress cap, probes within
       the reserve, and the `set_rtx_cache` pin;
     - the allocation tests: a low-average share with large keyframes on an
       unconstrained viewer, admission overflow, and deadline expiry.
   - **Voice acceptance:** the paired measurements and gate in "Voice headroom
     and acceptance", including the `LATENCY_NET` `rate=` option and the
     screen load generator.
   - CPU per Mbps measured on the VM, recorded here.

   Built in PR 3b, beyond the list: a 10 s ramp before a new slot's pauses
   count, the probing target capped at 1.5 times the estimate, stalled viewers
   reprobing every 15 s, Retry resuming bandwidth pauses, and the screen
   load and gate harness. Round-1 fixes add wire-byte original pacing/capping,
   tiny-delta-safe probes, priority reclamation, immediate keyframe recovery,
   atomic timestamped counters with incomplete coverage, paired voice roles,
   validated screen load, a per-pair tail gate and relay lateness measurements.
   The VM gate follow-up fixes padding-burst pacing and adds healthy-startup,
   scheduling-jitter and automatic-recovery regressions, per-viewer receive
   scheduling and explicit pause-trigger diagnostics.
   The ten-pair voice gate and the CPU figure on the
   VM are still to run ("Voice headroom and acceptance", "Cost on the
   deployment VM").
4. **Web sharing and watching, plaintext (L, 7–9 days).**
   - `voice/screen.ts`: publish and watch transports, the capture with presets
     and constraints, codec preference, sender parameters with uplink
     headroom, and receiver `jitterBufferTarget`;
   - the stage UI, LIVE badges in every visible channel, watching without
     joining voice, the sharing bar with the viewer count, fullscreen and
     PiP.
   - Unit tests for the state machine.
   - Playwright `e2e/screen.mjs` against a real local server:
     - Chromium sharer with **real tab capture** (`channel: "chromium"`,
       `--auto-select-tab-capture-source-by-title`, which spike S4 showed
       works headless);
     - WebKit sharer with a canvas track injected through the test hook;
     - viewers in both engines check decoded frames and a stamped barcode
       (the S3 technique);
     - late join, start → stop → start while watching two other shares,
       moderator stop and the slot limit;
     - a viewer in another channel watches without joining voice.
5. **Desktop enablement, plaintext (M, 3–4 days).**
   - The desktop UI runs the web screen transports and the screen worker
     beside the native voice engine. The native engine and `voice_set_keys`
     are unaffected.
   - The button is hidden on Linux (owner decision: deferred), and gated to
     macOS 14+ (owner decision).
   - CSP review: no change expected, since WebRTC is not governed by
     `connect-src`.
   - The native-voice Playwright run (`e2e:native-voice`) extended with a
     plaintext share watched by a browser, with native voice in the same call.
   - The manual checklist from PR 1 repeated on release builds.
6. **TeamSpeak watch links (L, 7–10 days).** Everything in
   [TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6): the token,
   `/ws/watch` and the guest transport, guest limits and revocation, the
   bridge's post, late-joiner message and presence marker, the watch page,
   the sharer's indicator, and tests:
   - bridge unit tests with the existing harness: the post goes through the
     puppet or signed ServerQuery like an attachment link, never in
     encrypted channels or with `file_access: members`;
   - server tests: an unknown, ended or revoked token answers the same
     `not_found`; the guest limit, the viewer limit and the per-IP limits
     hold; revocation disconnects guests; a guest cannot send anything but
     the watch ops;
   - Playwright: a guest page in Chromium and WebKit watches a share, then
     sees "The share has ended";
   - acceptance: the TeamSpeak notice and checkbox are shown before any
     token exists or any post is made. Nothing is posted before the sharer
     confirms, and nothing at all with the checkbox off;
   - a member's watch reclaims guest reservations when the budget is full;
     the paused guests resume only after passing admission again;
   - `/ws/watch` is held to the same `PreAuth` limits as `/ws`, IPv6 /64
     grouping included; a second `guest.offer` closes the connection;
   - the heartbeat: a guest page keeps watching for 150 s, more than two
     idle intervals, without being disconnected, and a guest that stops
     sending `ping` is closed after `IDLE_TIMEOUT`;
   - the bridge's link copy is gone after stop, revocation and detach.

**First release after PR 6: plaintext channels only, without sound.** The
owner chose to ship sharing in plaintext channels first and add encryption
afterwards, so PRs 1–6 are released before PR 7:

- In encrypted channels the server refuses `screen.start` (PR 2), and the
  button shows a disabled "coming soon" state. A share is never sent in the
  clear in an encrypted channel: no silent downgrade.
- PR 7 then enables encrypted channels. Until it lands, the controller work in
  "Two engines under one call controller" does not touch voice at all.

7. **Encrypted shares (L, 8–10 days).**
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
   - The server stops refusing `screen.start` in encrypted channels, and the
     button's "coming soon" state is removed.
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
       keyframe;
     - the native-voice Playwright run (`e2e:native-voice`) extended with an
       encrypted share watched by a browser, with native voice in the same
       call.

**Release acceptance (before the feature is announced)** is a manual matrix:

- sharers and viewers on Chrome, Firefox and Safari, and the desktop on
  macOS and Windows, in plaintext channels for the first release and again
  in encrypted channels after PR 7;
- a TeamSpeak watch link opened from TS3 and TS6 clients, in Chrome,
  Firefox and Safari;
- Firefox both shares and watches. Playwright's Firefox did not start in the
  spike environment, so this interop is checked by hand, or in CI if it runs
  there.

The first release (PRs 1–6) is about **9–12 weeks**. Phase 1 with encryption
(PR 7) is about **11–15 weeks** in total. Against the merged design's 6–8
weeks, the owner's decisions add:

- TeamSpeak watch links (PR 6): 7–10 working days, about 2 weeks;
- watching from outside voice (PRs 2–4): 2–3 days;
- the monthly egress counter (PR 3b): half a day.

Shared audio (6–8 working days) is not in these figures: it moved to phase 2
(PR 9).

Calendar weeks include review rounds, at the same ratio as before (about 1.4
times the working days). The estimate stays uncertain until three things are
known:

- PR 1 shows whether the WKWebView picker works without changes;
- PR 3b's voice gate passes or forces more isolation (its own thread or port);
- PR 7's controller change lands without reworking the voice activation
  tests.

### Phase 2: quality and reach

8. **Slow viewers without pausing (M, 4–5 days):** VP9 `L1T3` from Chromium
   sharers, and per-viewer temporal-layer dropping driven by the egress
   estimate. Pause stays as the last resort.
9. **Shared audio (M, 6–8 days):** everything in
   [Shared audio](#shared-audio-phase-2): browser tab audio by default,
   computer sound on Windows Chrome/Edge on request, and SFrame for it in
   encrypted channels.
10. **Desktop shared audio (M, 5–7 days):** native system-audio capture that
    leaves out Gwar's own process (macOS ScreenCaptureKit with
    `excludesCurrentProcessAudio`, Windows WASAPI process loopback excluding
    Gwar), fed into the webview's share as a track.
11. **H.264 with hardware encoding (M, 4–5 days):** NAL-aware SFrame with
    start-code escaping, and the "Game" preset prefers H.264 where the sharer
    has a hardware encoder (`powerEfficientEncoder` in stats).
12. **Pop-out (M, 3–4 days):** a desktop second window, and Document PiP in
    Chromium.
13. **Simulcast (L, 1–2 weeks)**, if temporal layers prove insufficient.

### Phase 3: native and peer-to-peer

The native part of this phase is now planned in
[native-media.md](native-media.md): native capture, hardware H.264 and higher
resolutions on every platform, Linux included, and shared audio that leaves
out Gwar's own voice. It replaces the first bullet below.

- **Native capture and encode in `gwar-client`:** macOS and Windows for games
  if needed; Linux only if the owner revisits the deferral. str0m frame mode
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
- **SFU CPU on the VM.** Extrapolated from an M5 (PR 3b measures).
- **Voice under video load.** Isolation is designed, not yet measured. PR 3b's
  paired gate decides whether the shared-runtime task is enough, or whether
  the screen task needs its own thread or port.
- **Controller complexity.** Two endpoint kinds under one MLS call controller
  is the riskiest change to already-reviewed encrypted-voice code (PR 7). It
  needs its own review round, as voice 5c/5d had.
- **WebView2 picker bugs** (#5173 size, #5763 deadlock).
- **Firefox:** no `contentHint`, no `scalabilityMode`, no audio. It works, but
  it is the lowest-quality sharer.
- **Voice echo through shared sound (phase 2).** Computer sound on Windows
  relies on `restrictOwnAudio` (Chrome 141+, still marked experimental on
  MDN). Gwar keeps a system-audio track only when the setting reads back
  true, and PR 9's acceptance checks it with a live call. The desktop app
  shares no sound until native capture can leave out Gwar's own process.
- **Lip sync (phase 2)** rests on arrival wallclocks at the SFU. If PR 9
  measures more than about 80 ms of skew, the SFU derives wallclocks from the
  sharer's sender reports instead.
- **Watch links leak like attachment links.** Anyone holding one watches until
  the share ends or the link is turned off. This is bounded and documented,
  not prevented.
- **Wider audience, more egress.** Watching without joining voice, and
  guests, raise the number of viewers per share. The 25-viewer limit, the
  budget and the monthly counter bound it.
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

## Owner decisions

Recorded on 2026-10-21. They replace the open questions of the first
version of this design.

1. **Linux desktop: deferred.** The Linux app has no screen sharing for now;
   Linux users share and watch from the browser. A native Linux path (portal
   and PipeWire capture, libvpx, str0m frame mode) remains possible later.
2. **Who can watch.**
   - In plaintext channels, anyone who can see the channel; being in its
     voice is not required.
   - In encrypted channels, only the call's participants (the keys come from
     the voice MLS group).
   - New: TeamSpeak users watch through a link posted in the bridged
     TeamSpeak channel, never for encrypted channels
     ([TeamSpeak watch links](#teamspeak-watch-links-guests-pr-6), PR 6, in
     the first release).
3. **Default permission:** every member may share (`share_screen` in the
   Member role); admins can remove it.
4. **Limits:** 4 shares per channel, 25 viewers per share (guests included),
   8 Mbps per share and a 300 Mbps server budget. A monthly egress counter
   warns admins against Oracle's 10 TB/month outbound allowance (one full
   share uses it in about 110 hours).
5. **Oracle: the account is Always Free** (owner, 2026-10-21). The free
   compute allowance is 2 OCPU / 12 GB of A1, as Oracle's documentation
   states (1,500 OCPU-hours and 9,000 GB-hours a month). No design change:
   phase 1 fits in 2 OCPU.
6. **Audio: optional for the first release, and moved to phase 2** (PR 9,
   [Shared audio](#shared-audio-phase-2)). The owner allowed it in the first
   release only if clearly simple and safe. It is not: even tab audio alone
   is 4–6 working days across both transports, the SFU, lip sync and the UI;
   the echo-safe default reaches only Chrome and Edge; and computer sound
   depends on an experimental Chrome constraint. Phase 1 keeps the
   transports, pacer and wallclock rule ready, so sound is added without
   rework.
7. **macOS 14+** for desktop sharing: accepted.

Smaller defaults chosen in this design, which the owner may change:

- watching a plaintext share in a password-protected channel needs being in
  that channel or `bypass_password`;
- at most 10 guests per share; the watch link is on by default and the
  sharer can turn it off before or during the share;
- the monthly egress counter only warns; it never stops watches unless
  `monthly_egress_stop` is set;
- in phase 2, "Mute the shared tab for me" starts off, until PR 9 measures
  echo.
