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
- **Plaintext browser codec: H.264 first, VP9 profile 0 as the only fallback.**
  A2 forwards H.264 mode 1 through the SFU.
  Encrypted shares are still refused by the server; their future SFrame
  layouts follow the spike results:
  - VP9 survives whole-frame SFrame.
  - VP8 needs its first 10 bytes (keyframe) or 3 bytes (delta) clear; they go
    into the AAD.
  - H.264 and AV1 send nothing at all when the whole frame is encrypted.

  A2 has landed with browser H.264, hardware where available; encrypted H.264
  also needs NAL-aware SFrame (B2/E2). AV1 + DD follows as A2b.
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
(portal and PipeWire capture, hardware AV1/H.264 or OpenH264 encode,
str0m frame mode) is the one way to give the Linux app sharing; the owner
deferred it.

## 2. Codecs and encoding

### Support

Browser capabilities below do not imply Gwar negotiates every codec.
The target follows the [2026-10-23 owner decision](native-media.md#native-codec-ladder-owner-decision-2026-10-23):
web H.264 preferred (A2 landed), VP9 as the emergency fallback, no VP8 or HEVC.
Native sending uses hardware AV1 → hardware H.264 → OpenH264; native VP9
decode stays for browser compatibility.

| Codec | Chrome send/recv | Firefox | Safari / WKWebView | Hardware encode (typical) | SFrame on whole frame (spike S3) |
| --- | --- | --- | --- | --- | --- |
| VP8 (dropped) | yes | yes | yes | rare | **breaks**: depacketizer cannot see the keyframe → PLI storm, 0 frames decoded. Works with 10/3 clear bytes |
| VP9 (target emergency fallback) | yes | yes | yes (on by default) | Intel/NVIDIA on Windows; none on Apple | **works** (keyframe and size are in the clear RTP payload descriptor) |
| H.264 (preferred, A2 landed) | yes (HW where available; absent in some Chromium builds) | yes (encoder needs OpenH264 plugin) | yes (VideoToolbox) | everywhere | **breaks**: packetizer sends nothing. Works with NAL headers clear (LiveKit also escapes start codes) |
| AV1 | yes (screen-content tools) | 136+ | yes (decode needs M3/A17+) | rare (newest GPUs) | **breaks**: packetizer sends nothing; needs OBU-aware SFrame (LiveKit: unsupported) |
| H.265 (not used or planned) | 136+, HW only | no | yes | Apple, recent GPUs | not tested |

**Browser codec rule (A2, plaintext shares):**

- Send H.264 `packetization-mode=1`, preferring Constrained Baseline
  (`42e01f` / equivalent constraint flags), then Constrained High or High;
  next VP9 profile 0, followed by RTX. Main (`4d001f`) and
  mode 0 are excluded. The SFU accepts Constrained Baseline, Constrained
  High (`640cxx`) and High (`6400xx`); it advertises level 5.1 receive support
  and `level-asymmetry-allowed=1`.
- Receive preferences come only from `RTCRtpReceiver.getCapabilities`, with
  H.264 and VP9. A sender capability never implies decoder support.
  The SFU selects each slot's PT from that viewer's negotiated codecs and
  H.264 profile/receive capability. The SPS supplies the actual stream's
  profile and level. Receive `max-recv-level`, `max-fs`, `max-mbps`, `max-br`
  and `max-dpb` are retained per offered PT; SPS levels and actual size/rate
  must fit that viewer. The publish answer
  advertises level 5.1 (`42e033` / `640c33` / `640033`) with mutual
  `level-asymmetry-allowed=1`, allowing 1080p60, 1440p60 and 2160p30 within
  the unchanged ingress frame/packet/byte bounds.
- Browser and guest viewers probe WebRTC H.264 decoding at 720p30, 1080p60
  and 1440p60 for each admitted profile. The highest verified tuple is sent
  in the bounded `h264` capability list. Without it, the SDP level remains
  authoritative; `browser: true` alone never promotes a decoder. Explicit
  resource ceilings still apply. Every SPS, including every aggregate SPS
  and bounded reassembled FU-A, is checked against profile, level and coded
  macroblock size. A two-second RTP timestamp window enforces max-mbps with
  10% tolerance without treating arrival jitter as an overrun.
  See [Browser interop](native-media.md#browser-interop) for the wire fields,
  dimensions and recovery rules. The fixed-canvas screen e2e requires Game
  1920×1080 H.264 at the sender and Chromium/WebKit receivers using real decoded
  dimensions. CI also checks adaptive real-tab capture with the selected codec
  and moving barcode, without requiring the capture to retain its maximum size.
- A viewer that cannot decode a share gets its slot freed and
  `screen.stopped {reason: "codec"}`, with a clear browser/guest-page message.
  The share and other viewers continue. There is no transcoding or native
  compatibility layer in A2. The decode watchdog isolates one failing viewer
  after a two-second corroboration window. Only failure reports from every active
  H.264 viewer beyond the four-second watchdog window trigger share-wide recovery:
  a lower H.264 target, then VP9 when all peers offered it. Reports are limited
  per viewer/share to one every ten seconds; the share has a ten-second hold-down.
  Watch slots survive replacement, old-publisher packets drop, and the replacement
  keyframe opens the retained slots. Restore H.264 after reporters leave or two
  minutes, then hold for a minute before another downgrade. A3/B11a own native
  compatibility layers. An explicitly limited native decoder may still need 720p.
- Hardware encoding is chosen by the browser where available, with its
  software H.264 encoder otherwise. The sharing bar shows the negotiated
  codec and exposes the encoder implementation/hardware hint in its tooltip
  when `getStats` reports them. Presets still set `contentHint`,
  `degradationPreference`, frame rate and bitrate independently of codec.

Encrypted H.264 still needs NAL-aware SFrame and start-code escaping, as
specified in [native-media.md](native-media.md#encryption-nal-aware-sframe).
A2 does not enable encrypted shares, browser AV1, HEVC, or native clients.
VP8 is dropped. Verified H.264 decode probes and bounded recovery are described
in [native-media.md, Browser interop](native-media.md#browser-interop).

### Presets

The user picks a preset. "Auto" is the default: Text for windows and tabs,
Smooth for screens; Chromium's `displaySurface` setting tells which.

| Preset | Resolution cap | fps cap | `contentHint` | `degradationPreference` | max bitrate |
| --- | --- | --- | --- | --- | --- |
| Text (code, documents) | 1080p (native if smaller) | 15 | `detail` | `maintain-resolution` | 2.5 Mbps |
| Smooth (default for screens) | 1080p | 30 | `detail` | `balanced` | 4 Mbps |
| Game | 1080p | 60 | `motion` | `maintain-framerate` | 8 Mbps |
| Saver | 720p | 30 | `detail` | `balanced` | 1.5 Mbps |

Game uses `maintain-framerate` from startup. The trial two-second resolution hold,
even with a bounded early exit, delayed cadence and stable dimensions on shaped
links, so the implementation uses a stuck-capture guard instead. There is no
persistent sender size bound or resolution repricing controller. The capture caps,
bitrate ceiling, priority and voice headroom remain the same; Text preserves
resolution and Smooth/Saver keep `balanced`.

For a resolved `maintain-framerate` spec, the guard can reset Chromium's pending
capture adaptation with a one-second `maintain-resolution` pulse, then restore the
current spec using fresh sender parameters. It requires six continuous seconds of
undersize, no CPU limitation, and both the encoder target and 70% of a fresh outgoing
estimate above `larger pixels × max(15, source fps) × bpp × 1.5` bits/s. The target,
cadence and cap come from the live resolved spec, including effective codec-recovery
limits. The retained settings target cannot exclude attempts toward that spec (L1).
Any positive output below the spec is eligible. The larger step is capped at the
spec and otherwise four times the current pixels, with a 720p floor. The bpp budget is at least 0.04 and grows
with measured payload demand, leaving genuinely expensive content alone.

A low-QP escape handles a stalled encoder spending most of its bitrate target at a
small size. It requires output below half the effective target pixels, larger settings
within this capture identity or the live-spec fallback (L1), and at least six seconds /
60 encoded frames of consecutive positive `qpSum` / `framesEncoded` deltas. With that
QP evidence, affordability uses the nominal next recovery step toward the live spec,
without extrapolating payload demand from the adapted size. H.264 uses QP ≤24 on its
0–51 scale; VP9 uses ≤96 on its 0–255 scale. Both counters must advance, the ratio must
be plausible, and outbound id, encoder implementation and codec must match. Missing QP
or identity, counter/geometry changes and polling gaps reset the window. The escape retains the nominal bpp floor, 50% margin and fresh
estimate / CPU exclusions. See [startup validation](screen-startup-validation.md) for
threshold justification and three-repeat cold-browser measurements.

Retries retain 20, 40, 80 and 120 second backoff. Every pulse is judged across its
restore using temporary 250 ms sampling: success needs ten seconds of larger
output with fresh encoded-frame progress after restore. Duplicate reads do not
restart that clock; missing or frozen counters cannot establish recovery. Two unsuccessful
attempts suppress further pulses; the QP escape is one-shot per cycle. **L2:** all
suppression expires only with thirty continuous seconds of healthy capacity for the
**next recovery step**, without CPU limitation. Full-target capacity is unnecessary:
a 270p20 capture can retry with a 1.75 Mb/s encoder budget sufficient for the nominal
720p20 step (1.106 Mb/s), even if the retained 1080p20 target needs 2.488 Mb/s.
Successful recovery discards the old outcome size reference. Any fresh encoded-frame
output improvement during an attempt resets the non-improving-cycle backoff; the
stronger sustained 50% gain criterion still decides fuse success.

The target model is asymmetric: losing a recovery target can strand capture forever,
whereas excess attempts are bounded. Each surface identity retains a **ceiling** of
its largest observed settings width and height, independently per axis. This can
combine dimensions from different aspect ratios, deliberately preferring an
overestimate. The ceiling never shrinks for window resizes, capture-stage scaling,
CPU/bandwidth adaptation, pulses, application caps, or delayed cap frames, even with
limitation `none`. The guard target is `min(ceiling, live spec)` per axis; the spec
includes recovery caps. Caps never overwrite the ceiling, so relaxing one restores
the full target immediately. **L1:** the ceiling never vetoes an attempt toward the
live spec. Growth and Chromium adaptation can both occur between polls, or a new
identity can first appear already adapted; neither needs a larger observed snapshot
to remain recoverable. The live spec supplies the independent liveness fallback.

Identity is the publisher's track plus the synchronous `getSettings()` fields
`deviceId` and `displaySurface`. A new track creates a new publisher. A change to a
previously known field cancels the previous surface's pulse/outcome and resets its
guard/QP history **before** seeding a new ceiling from that same settings snapshot.
Stats dimensions and media-source ids cannot seed or reset the ceiling: stats can
lead or lag settings and may still describe the previous identity. Until synchronous
settings identify a switch, they belong to the previous identity. Missing identity
fields or switches between indistinguishable surfaces retain the higher ceiling;
there is no claim to prove physical dimensions. A switch first observed already
reduced seeds its ceiling from those settings, with the spec fallback eligible
immediately. It does not need spontaneous growth to enable pulses.

**L3:** consecutive non-improving attempt cycles use **10 → 20 → 40 → 60 minute**
retry envelopes, capped at sixty minutes, measured from the final pulse in the
preceding cycle. One ordinary two-second polling interval is reserved inside each
envelope for observing its completion; continuous eligibility and QP evidence survive
suppression. Identity switches, preset/spec changes (including codec recovery caps),
and fresh-frame improvement reset this backoff to ten minutes. Ceiling growth alone
never resets it. With continuously healthy next-step capacity, valid advancing
telemetry and ordinary polling, output below the spec gets a pulse within **at most
60 minutes**, including that polling interval. A fresh unfused guard needs six seconds
of eligibility (at most eight seconds including its first ordinary poll). CPU pressure,
unaffordable payload demand, invalid telemetry and polling stalls do not satisfy this
liveness premise. The valid low-QP escape retains its evidence requirements.

For a permanently smaller, non-growing surface under an unchanged identity and
configuration, every pulse fails: **at most two issued one-second pulses in any
half-open ten-minute window**. Once the sixty-minute cap is reached, the bound
sharpens to **two pulses per half-open hour**. If output repeatedly gains 50% for
ten seconds and regresses, even an
overestimated target can record successes: the unchanged 20/40/80/120-second attempt
spacing bounds these to **seven pulses per ten-minute window** (at most five once
spacing reaches 120 s). Explicit surface/preset/codec resets start fresh history;
including arbitrary such resets, six-second eligibility bounds a publisher to
**100 pulses per half-open ten-minute window**. The one-second deadline is independent of sender promise completion, with serialized current-state
restores.
Preset changes, close and codec replacement fence queued parameters and cancel
phase/sampling timers. `scaleResolutionDownBy` only enforces the existing
codec-recovery ceiling from current track settings.

`pnpm --filter web e2e:screen-startup` checks repeated fresh real-tab publishes
with a sparse 20 fps barcode and busy 1080p60 content, using VP9 and H.264. It
checks rendered and captured cadence, outbound resolution and viewer decoding
through the local SFU after settling. `E2E_STARTUP_RESULTS` retains JSON timelines;
`E2E_STARTUP_UPLINK=1500000` or `3000000` exercises Chromium's global p2p shaper
and reports the constrained-link tradeoff.
`E2E_STARTUP_PRESET` can check Text, Smooth and Saver directly. The Ubuntu
`browser-screen-startup` job is diagnostic and excluded from `ci-ok` until its repeatability is established.
The existing required #166 fixed 1080p60 H.264 canvas gate is unchanged.

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
- **Codecs:** H.264 mode 1 (Constrained Baseline/Constrained High/High),
  VP9 profile 0, with RTX; no AV1 or HEVC negotiation yet.
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
- **PR 3b acceptance calibrates voice under screen load against A/A on the same
  host**, using the `latency` example and `LATENCY_NET` relay from docs/latency.md:
  - The relay gets a `rate=` bottleneck option with a bounded queue. The
    example gets a screen load generator: a str0m frame-mode client publishing
    synthetic VP9-sized frames at a preset's rate, through the same relay, and
    a viewer receiving it.
  - Scenarios: unconstrained loopback with a 1080p60 share at 8 Mbps and five
    viewers; a 10 Mbps sharer uplink with an 8 Mbps share; a 5 Mbps viewer
    downlink watching two 4 Mbps shares; each also with 1 % loss.
  - Report voice p50/p95/p99 mouth-to-ear, lost and late frames, playout
    underruns and concealment, as the latency docs do. Eight pairs **per set**
    and 100 markers per run by default; A/A and screen sets are interleaved.
  - Gate: each screen-set metric must be no worse than the same A/A metric
    plus the tolerance defined below. Missing the calibrated gate blocks PR
    3b until the isolation is changed (own thread, separate port or a lower
    video allocation).

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
when the reserve is ready. The extras bucket capacity is unchanged.
Regressions use an independent viewer clock with 1, 2, 5 and 10 ms SFU
cadences plus periodic 10 ms stalls, require steady video throughput, restore
a link after an actual bandwidth pause, and pin resume at exactly 10 s of
continuous room. Pause thresholds and socket caps are unchanged.

**Historical follow-up verification at `674633d`.** All builds used `CARGO_BUILD_JOBS=4`
and the worktree's `target/`. Server unit tests: 287 passed (including 51
screen tests and monthly egress); screen integration: 10; latency example:
27; vendored str0m: 759 unit tests and 14 probe integration tests, including
padding disable/reset/re-enable; Python harness: 9. The no-default-features
server check, clippy with `-D warnings` for the server/proto/client and vendor
library/probe regression, rustfmt and `git diff --check` passed.

The `simple::bwe_cellular` failure was caused by Gwar's generic immediate-timeout
patch, not by unchanged crates.io str0m 0.24.1. Reproducing it at `89dacfe`
compared two revisions that already contained the faulty patch. Paced media
now keeps upstream's one-microsecond clock advance, including during probes.
Only application-paced media and generated padding bursts drain at the same
clock. Scheduler-aware first-burst sizing applies only to application-paced
streams. The cellular regression passes without lowering its thresholds.

**Previous review-fix validation (`5628c4c`).** All builds used `CARGO_BUILD_JOBS=4` and the
worktree's `target/`. The server's 288 unit tests passed, including 52 screen
and egress tests and every recovery-matrix case; screen integration: 10;
latency example: 27; Python harness: 19. The full vendored str0m suite with
`--no-default-features --features aws-lc-rs,_internal_test_exports` passed:
765 unit tests, 330 integration tests (including `simple::bwe_cellular`), and
59 doctests; two doctests remain ignored. The no-default-features server
check, server/proto/client and full-vendor all-targets clippy with
`-D warnings`, workspace/vendor rustfmt, and `git diff --check` passed.
No deployment host or VM was contacted.

**Recovery follow-up validation.** All builds used `-j 4`. All 290 server unit
tests passed, including the 70-case matrix, both exact review repros and six
parameterized variants. Server unit test code used package `opt-level=1`,
with debug assertions and overflow checks enabled. Screen integration: 10;
latency example: 27; Python harness: 19. Full vendored str0m with
`--no-default-features --features aws-lc-rs,_internal_test_exports`: 769 unit
tests, 330 integration tests (including `bwe_cellular` and capacity restoration),
and 59 doctests passed; two doctests remain ignored. The required
`cargo check -p gwar-server --no-default-features --tests`, server test clippy
with `-D warnings`, workspace/vendor rustfmt and `git diff --check` passed.
The full vendor run used canonical ignored capture fixtures from the locally
cached str0m 0.24.1 package; those scratch copies are removed after validation.

**Review follow-up after rebasing onto `main` at `a78f775`.** A plain revert of
the BWE fixture origin edit passed ten consecutive cellular runs, but the second
full vendor run failed capacity restoration at 644.878 kbps against its unchanged
1 Mbps assertion. The final fixture configures one origin and 100 us resolution
before the handshake, advances through the 100 ms startup boundary without
reassigning live clocks, and schedules BWE's forced wakes from the current input
instead of an older future deadline. Other vendor fixtures retain their existing
scheduling. No BWE assertions, thresholds or production estimator code changed.
The final cellular and capacity-restoration tests each passed ten consecutive
isolated runs. Both complete vendor runs passed: 770 unit tests, 330 integration
tests and 59 doctests per run, with the existing two doctest ignores.
Post-rebase validation also passed: 335 server unit tests (all 70 recovery-grid
cases, 60 zero-pause startup cases and the outage repros/variants), 10 screen
integration tests, 15 watch-link integration tests, 27 latency tests, 120 protocol
and binding-generation tests, and 19 Python tests. The no-default-features server
check, server/proto/client and full-vendor all-targets clippy with warnings denied,
workspace/vendor formatting and `git diff --check` passed. Builds used `-j 4`;
server test code used `opt-level=1` with debug assertions and overflow checks.

The earlier macOS loopback gate at `a4c76a1` used one pair of 40 markers
per scenario:
unconstrained, all five viewers at 7.87 Mbps with zero pauses,
voice p95/p99 deltas +0.60/+0.25 ms;
downlink-loss, 3.08 Mbps still active, p99 delta
+14.92 ms. Both passed with zero underruns in baseline
and screen runs. The final startup-stall replay kept every viewer at
7.81 Mbps with zero pauses despite 13.78 ms maximum
receive wake lateness; the original replay paused two viewers permanently.
These shortened local runs do not replace the calibrated six-scenario VM
gate or its CPU scaling measurement. No deployment service or VM was contacted.

**As built in PR 3b** (docs/latency.md, "Screen load"; `scripts/latency/screen_gate.py`):

**Owner-approved A/A calibration.** On the Oracle aarch64 deployment host
(4 vCPU), the supplied `89dacfe` unconstrained A/A set (ten pairs, 100 markers)
missed every old absolute rule with no video in either arm: median p95/p99
deltas +5.567/+4.849 ms, six tail pairs, and 3 → 8 underruns. Individual
latency deltas ranged from about −37 to +41 ms; playout underruns introduced
steps of about 14.6 ms. The supplied `a4c76a1` screen set was better:
+0.009/+0.151 ms, two tails, and 1 → 2 underruns. These historical sets motivate
calibration; they are different revisions and separate invocations, so they
are **not** a calibrated acceptance result. Artifacts remain in
`/tmp/gwar-gate-aa-89dacfe/` and `/tmp/gwar-gate-a4c76a1/`.

- Each scenario collects two equal-sized sets in **one invocation of the same
  binary on the same host**. A/A has no video in either arm; the screen set
  has a baseline without video and a second arm with the scenario's load.
  Both use the same pair-index seeds, voice impairment roles, 15 s warm-up,
  marker count and alternating arm order. The second A/A arm retains the
  artifact label `screen`; its set name is `aa` and its screen metadata is null.
- One block contains four fresh processes: for even zero-based pair indices,
  `aa/baseline → aa/screen → screen/baseline → screen/screen`; odd blocks
  reverse both set order and arm order. The four runs share seed `index + 1`.
  Thus matched sets are adjacent and slow host drift has balanced order over
  each two blocks. Even pair counts are recommended for balance. Sets never
  run concurrently and there is no retry, outlier removal or seed selection.
- For each set, compute signed per-pair deltas (second arm − baseline), then
  their medians for p95, p99 and underrun fraction. Underrun fraction is
  underrun samples / samples played over the harness's full run, including
  warm-up, identically in both sets. Also compute **total underrun excess**:
  sum of second-arm underrun counts minus sum of baseline counts. Negative
  A/A metrics remain signed; no absolute value, clipping to zero, or borrowing
  a calibration from a different scenario or invocation is permitted.
- **Unconstrained:** screen median p95 delta ≤ A/A median p95 delta + **2 ms**;
  likewise p99; screen total underrun excess ≤ A/A total underrun excess
  (**zero** extra-count tolerance).
- **Constrained:** screen median p99 delta ≤ A/A median p99 delta + **20 ms**;
  screen median underrun-fraction delta ≤ A/A median fraction delta +
  **0.005** (0.5 percentage points, not 0.5 percent relative).
- **Both:** screen tail-pair count ≤ A/A tail-pair count + `floor(N / 10)`.
  A tail pair exceeds any original absolute per-pair rule: unconstrained,
  p95 or p99 delta > 2 ms or any excess underrun count; constrained, p99
  delta > 20 ms or fraction delta > 0.005. Count the union once per pair.
  At eight pairs there are **zero additional tails** above A/A; at ten, one.
  This keeps intermittent starvation visible even when the median is unchanged.
  All applicable comparisons must pass; equality passes. Median latency is
  still reported as a diagnostic, without a separate gate.

**Statistical interpretation.** This deliberately uses a deterministic
comparison of descriptive metrics, rather than a bootstrap/permutation
p-value. Eight or ten pairs and rare underrun steps provide little information
about the tail distribution; a bootstrap cannot recover unseen steps, while
"no significant difference" in a low-power test would not establish that a
regression is within tolerance. An upper confidence bound for noninferiority
would be preferable for a powered experiment, but at this sample size it
would often be inconclusive even for A/A. The chosen rule implements the
owner's empirical noise allowance without presenting it as 95% confidence.

It reduces false failures from host noise already visible in interleaved A/A.
Finite-sample differences can still falsely fail two equivalent sets,
especially signed underrun counts with zero tolerance. Conversely, unusually
bad A/A can mask a real screen regression, and eight pairs can miss rare
starvation. Interleaving and balanced seeds/order reduce drift bias; the
separate tail/count checks catch regressions a median hides. They do not
remove sampling uncertainty or establish an absolute latency bound. Preserve
both sets and every failed run, investigate suspicious calibration, and do
not rerun selectively until a pass. `--null` collects two interleaved A/A
sets for repeatability diagnosis; it cannot establish acceptance under load.

**Output and VM command.** `summary.json` records `sets.aa` and `sets.screen`
side by side within every scenario: all pairs, median deltas, arm underrun
totals, underrun excess and tail count. `comparison` lists A/A, screen,
tolerance, limit, screen-minus-A/A and pass for each applicable metric;
`checks` and `pass` give the scenario verdict. `execution_order` pins seeds
and ordering; each set has its own manifest, CSVs, JSON and logs in
`<scenario>/aa/` or `<scenario>/screen/`. Run/load errors are recorded and
leave the overall verdict false.

From the checked-out PR head at the repository root on an otherwise idle VM:

```sh
export CARGO_BUILD_JOBS=4
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release -p gwar-client --example latency
python3 -B -m unittest discover -s scripts/latency -p 'test_*.py'
python3 -B scripts/latency/screen_gate.py \
  --binary target/release/examples/latency --pairs 8 --markers 100 \
  --out "/tmp/gwar-gate-calibrated-$(git rev-parse --short HEAD)-$(date -u +%Y%m%dT%H%M%SZ)"
```

The example embeds a fresh loopback server; a standalone server build is not
needed for this command. Do not add `--null`. All six scenarios run by default. For a targeted downlink rerun, add
`--scenario downlink --scenario downlink-loss`; the repeatable selector preserves
all calibration, load and tail rules. A shared-pacing change also needs an
uplink sanity run (`--scenario uplink --pairs 4 --markers 100`) in a separate
output directory.
On a 4-vCPU host, ten pairs **per set** means 240 processes. The measurement
window is `1 + 15 + 0.25 × markers + 2` seconds = **43 s** at 100 markers:
**2 h 52 min** before setup/teardown, about **3.2 h** allowing five seconds
per process for startup, connections and shutdown. The recommended/default
**eight pairs per set** means 192 processes, **2 h 18 min** of measurement,
about **2.56 h (2 h 34 min)** with the same allowance. These are planning
estimates, not measured timings on this head; build time is additional and
host scheduling can extend them. Reducing pairs by 20% preserves even order
balance and 100-marker p95/p99 sampling, with 800 markers per arm/set/scenario;
it weakens rare-event coverage. Using fewer than 100 markers would make p99
still more dependent on the maximum marker, so the marker count stays 100.
Set `--pairs 10` for the longer run if the VM time budget permits.

**Load validity and diagnostics:**

- Every screen run must report the expected load spec and every expected viewer
  must receive at least 60 % of the scenario's intended steady media rate
  (8 Mbps for unconstrained/uplink; one 3 Mbps share for downlink), over at
  least one settled second. Unconstrained runs must also report **zero pauses**,
  including startup; even a resumed pause fails. Missing pause metadata fails.
  A missing, failed or quiet load fails the run; load-task errors propagate
  through the example's nonzero exit and are never calibrated away.
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
- **Historical results under the old absolute rule** (2026-10-22, Apple
  silicon laptop with 10 cores, shared
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
- **Left for the VM:** the calibrated six-scenario run above, which decides
  the gate, on a host that runs nothing else. If the outlier mode shows there,
  the next step is the isolation the gate names: the screen task on its own thread,
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
    when reclaiming every guest reservation would still be insufficient does
    the member get `busy`; a refused request leaves existing guests alone.
    Members also reclaim guest viewer slots on a full share before returning
    `busy`. Paused guests keep counting toward the 10-guest connection limit,
    but hold neither a viewer-slot reservation nor a live media grant.
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
    header byte. H.264 uses RTP timestamps for frame boundaries and A1
    NAL observations for a confirmed SPS/PPS/IDR prefix.
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
- **H.264 joins and recovery hold only a bounded prefix:** at most 8 packets
  and 16 KB of the newest RTP timestamp, in source sequence order. A complete
  SPS, complete PPS (including uninterrupted FU-A completion), then an IDR
  start must be present without gaps before a waiting slot opens at the SPS.
  Marker-before-repair, a new timestamp, a non-IDR slice, malformed NALs or
  a prefix overflow fail the candidate and request a throttled PLI. Active
  segments receive on arrival. Held bytes/packets are metered on arrival,
  including while ingress is blocked; replay is never charged again. The
  same frame bounds, stable RTP mapping, queues, pause/resume and recovery
  rules apply as for VP9. There are no H.264 spatial layers.
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

The SFU cannot observe every downstream queue. It smooths all screen RTP before
transmission, reserving voice headroom in ordinary traffic and bounding probe
rate and duration; the calibrated downlink gate measures the shared bottleneck.

**Why str0m's pacer cannot be the queue:**

- with BWE enabled, `write_rtp` queues video into str0m's leaky-bucket pacer;
- when the queue grows, the pacer raises its rate to drain within 2 s, rather
  than expiring packets (`pacer/leaky.rs`, `queue_limit`);
- the queue cannot be cleared through the public API;
- `queue_info()` is a cached sample.

**Original packets: a Gwar-owned queue.**

- Every forwarded video `StreamTx` gets `set_unpaced(true)`. The pacer then
  bypasses its ordinary queue-draining rate policy for those packets
  (`pacer/leaky.rs`: "Unpaced packets … are sent immediately").
  - TWCC, BWE updates and probe eligibility stay active (`session.rs`).
  - str0m uses this mode for audio by default.
- Packets wait in the screen task instead, in **one queue per viewer**, kept
  per slot.
  - One leaky bucket per viewer releases them at that viewer's whole budget,
    serving slots in strict priority order ("Bandwidth allocation per viewer"
    below).
  - Released packets go to `write_rtp`; a shared transport pacer then smooths
    them together with repairs and probes before TWCC send times are assigned.
    Originals are checked against their arrival deadline again at the socket.
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

  The default probe ceiling is **1.2 × estimate**. The highest-priority share's
  demand can raise it to at most 2×; lower-priority shares cannot force that
  increase. This is a combined RTP rate: media
  sent during a probe consumes its budget instead of adding to padding.
  Screen-watch probes use a nominal 10 ms target (two 5 ms wakes), retaining
  the five-packet requirement and distinct send timestamps. Coarse clocks allow two regular wakes
  plus 2 ms, still within the existing byte reserve, for usable timestamps. Their purpose
  is to test capacity above the estimate, so a short probe can temporarily
  consume voice headroom; it cannot send a whole frame at line rate.

**Per-viewer pacing and egress cap: originals, repairs and probes together.**

Each watch transport enables the vendored `Bwe::set_egress_pacing` before its
first probe. This adds a shared wire-byte debt to str0m's pacer, including
streams marked unpaced, RTX and generated padding. It operates before SRTP
output and TWCC accounting; neither delayed encrypted datagrams nor false send
timestamps are needed. Ordinary screen RTP is limited to **0.8 × estimate**;
short probes normally use **1.2 × estimate**. The actual ceiling is
`min(2 × estimate, max(1.2 × estimate, N / 0.8))`, where `N` is the largest
of 1.2× the highest-priority share's measured average and its largest keyframe
divided by 400 ms, all in bits per second. Before any media has been measured,
1.1× its admission cap supplies the bootstrap demand. That cap is the server's
ceiling, not the encoder's rate; continuing to probe for it would repeatedly
overload a smaller downlink even after its priority share fits. This discovers
keyframe room without letting a second share that cannot fit drive startup
bursts. Queue pressure cannot raise these limits. Normal bursts allow 2 ms of
the rate, with a one-MTU floor and one packet of rounding. Regular scheduler
cadences up to 20 ms are learned only while RTP is queued: idle frame gaps
cannot enlarge a later burst, and long stalls do not earn credit. Queue counts
are reduced after each send. Pacing wakes when the next packet becomes eligible,
and cadence learning uses lateness against that deadline rather than treating an
intentional debt-drain wait as scheduler delay. Otherwise the pacer can inflate
its own bursts even with a fine runtime clock. Voice has its own transport and
never waits in this screen pacer. Control traffic bypasses it.

Pacing and TWCC use serialized SRTP bytes plus 48 bytes for IPv6/UDP, also on
IPv4. Previously probes and BWE counted payload bytes while the socket cap
counted serialized bytes. Small padding packets therefore understated occupied
capacity; the resulting estimate, multiplied by the allocator's 0.8, could
incorrectly pause a fitting 3 Mbps share on a 5 Mbps link. Unpaced repairs and
probe bursts could also queue voice despite an acceptable average bitrate.
No random-loss exemption or minimum estimate is imposed: delay and loss BWE
still detect actual congestion and collapse. A successful probe replaces both
the loss-limited estimate and its state, as in WebRTC
`LossBasedBweV2::SetBandwidthEstimate`; the previous decreasing/HOLD state
cannot silently reapply its old cap to newly demonstrated capacity. Paced probes
with a local send gap above 20 ms are discarded as scheduler-stalled samples;
they cannot turn a delayed callback into a capacity collapse. Known lost sends
participate in this gap check, so random network loss does not masquerade as a
local stall. Regular sends that queue downstream remain valid congestion samples.
The setting and probe ceiling survive estimator resets and track new estimates.
An active short probe finishes its dispatched plan: changing its rate in response
to its own partial feedback would turn application backoff into a false capacity
measurement. Later clusters use the updated ceiling.

The explicit screen ceiling also marks these transports application-limited for
loss estimation and periodic discovery. An 80% policy ceiling is above the
automatic ALR detector's 65% threshold; an ACK rate under that policy is not a
measurement of network capacity. Probe-SSRC bytes do not count as application
media. An unsaturated underfilled probe can raise a capacity floor, but cannot
lower a prior estimate or reset a genuine loss-limited state without new
capacity evidence. Slower reception and ordinary delay/loss feedback still
reduce the estimate on actual congestion.

Application-limited ACK throughput is not used as a capacity candidate. The
previous maximum-of-probes tracker also no longer supplies a hard upper bound:
proving one rate says the link can carry at least that rate, not that it cannot
carry more. Delay and loss bounds still constrain growth and detect collapse.

Gwar retains its independent socket-volume backstops. Each outgoing datagram is
classified by its clear header, without decryption:

- **Originals:** RTP on a slot's main SSRC. The Gwar queue admits and releases
  conservative serialized bytes at `B = 0.8 × estimate`. Its admission and
  share meters reserve 64 bytes per packet for RTP/SRTP overhead. Release
  credit retains one 1264-byte packet beyond its regular 10 ms refill
  window, with a 2400-byte floor, so fractional packet credit is not clipped on
  every wake. Otherwise a 3.7 Mbps allocation can deliver only 2.8 Mbps at a
  10 ms cadence. The window expands to 30 ms when scheduler wakes exceed 10 ms.
  The socket bucket checks actual SRTP bytes at `B` and adds one further
  1264-byte packet of rounding: the shared pacer can mix originals and
  RTX at different burst boundaries from the release queue. It cannot send
  beyond the shared wire-rate ceiling. An original that reaches
  its 400 ms deadline after release is discarded at transmission, its lane is
  cleared and recovery uses the existing keyframe budget and publisher throttle.
- **Extras:** all other RTP (RTX and probe SSRCs). One viewer-wide socket
  bucket allows `0.1 × estimate`, with burst `max(64 KB, 30 ms × 2 × estimate)`.
  This is a volume reserve, **not permission for an instantaneous socket burst**:
  the shared pacer spreads it over time. Background padding is suspended while
  the reserve refills. Probe targets include header and packet-rounding room.
- **Control:** RTCP, STUN and DTLS are sent immediately.

Socket-volume accounting excludes UDP/IP; the pacer and BWE include it. The
long-term socket-volume bound remains `0.9 × estimate` plus control and initial
bucket credit, while the shared wire pacer limits ordinary RTP to 80 %. Originals
and repairs share that paced capacity, preserving 20 % for voice and other media
even while repairs overlap a frame. A rejected original clears its lane and counts its failed keyframe;
a rejected extra can be retried by NACK or later probing. Pending BWE events
are drained before originals are released, so both layers use the current
estimate. Socket drops can reduce BWE through TWCC loss; accurate pacing avoids
manufacturing those losses during ordinary probe and repair traffic.

The small-packet protection remains: cached payloads smaller than 240 bytes
are not reused for probes; blank padding is sent instead. They remain available
for NACK repair. See `vendor/str0m/GWAR-PATCH.md`.

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
delay). The relay's `rate=` serves the voice acceptance instead.

**Deterministic simulation and CI.** `Sim::with_seed` seeds str0m's thread-local
non-cryptographic RNG before either peer is built, including MID, SSRC, sequence
and ICE IDs. The test-only `_gwar_sim` feature orders stream iteration by SSRC
and maps one process-local `Instant` anchor to a fixed Unix/NTP epoch. All later
time comes from explicit simulated durations. Link loss keeps independent,
explicit seeds per direction. The remaining Gwar maps are keyed lookups, not
scheduling order. DTLS certificates, keys and signatures retain secure entropy;
the handshake completes before media, and replay comparisons exclude control
ciphertext/handshake byte counts. The replay regression compares every BWE
sample, media send/receive trace, allocation change and media counter for the
same seed, and checks a second seed changes the trace.

The main-CI re-pause after PR #144 was an allocator recovery defect. Seed 1
reproduces the exact 56.95 s resume, 59.66 s re-pause and 71.75 s second resume.
Resuming after all-paused silence resets BWE, but previously retained the
expired assignment ramp. Startup feedback under 20 ms scheduling, periodic
150 ms stalls and 2% loss briefly lowered the estimate from about 21 Mbps to
8.4 Mbps. Two failed keyframes then paused otherwise recoverable media. Every
estimator restart now renews the existing 10 s discovery ramp for active lanes.
The 400 ms deadline, queue admission, pacing and socket caps still apply, and a
unit regression verifies a real constraint pauses again after discovery ends.
The no-repause and received-throughput assertions are unchanged.

Default tests retain representative collapse/startup cases and all 1/5/10 ms
voice cadences. The full 96-case voice, 60-case startup and 70-case collapse
matrices are ignored only in the ordinary workspace invocation. CI runs all of
them in three `rust-screen-sim` shards, required by `ci-ok`, with compilation in
a separate step. Run a full shard locally with:

```sh
cargo test -j 4 -p gwar-server --no-default-features --lib full_voice_headroom_matrix -- --ignored
cargo test -j 4 -p gwar-server --no-default-features --lib full_startup_matrix -- --ignored
cargo test -j 4 -p gwar-server --no-default-features --lib full_recovery_matrix -- --ignored
```

Server tests use package `opt-level=1`, with debug assertions and overflow checks
still enabled. The 1 ms voice case keeps its 10 ms bound; the 5/10 ms steady and
startup bounds remain the existing cadence-plus-margin formulas. VM gate
thresholds are unchanged.

Local validation of this fix used `-j 4` and a disposable target directory under
`/tmp`. The unchanged intermittent-stall test passed 20/20 isolated process
runs (140 restored-link checks), with identical recovery traces. Three additional
isolated replay tests produced the same media/estimate fingerprint. The seeded
pre-fix test failed 3/3 times with the main-CI pause sequence. The complete
default-feature server unit suite passed 391 tests in 51.12 s using four test
threads, with the three full matrices explicitly ignored there. All full matrices
passed separately: 96 voice cases in 58.49 s, 60 startup cases in 36.20 s and
70 collapse cases in 284.31 s. These are local timings, not GitHub runner timings.
The 95 default screen unit tests, 10 screen integration tests, 15 watch-link
integration tests, 11 CI-selection/aggregate tests, workspace/vendor formatting,
server all-targets clippy with `-D warnings`, and
`cargo check -p gwar-server --no-default-features --tests` passed. A production
library check with the simulation feature disabled also passed. The temporary
target is removed after validation; no VM was accessed.

Results and deviations:

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
  capped estimate does not trail the uncapped estimate by more than 10 %.
  Capping can improve feedback by preventing probe loss. The
  separate recovery regression requires resumed media above 60 % of its
  intended rate. Built (`probes_after_a_collapse_fit_the_reserve`), with a
  finding fixed after the `07e2f24` VM gate: the probe pacer waited between
  individual padding packets instead of draining each generated burst. A
  240-byte blank packet per 2 ms wake limited discovery to about 1 Mbps even
  on a healthy link. The local str0m patch now drains each burst immediately
  and spaces only successive bursts on transports without the shared ceiling.
  Watch transports now queue a complete cluster for one wire-byte pacer, rather
  than having padding generation and transmission each pace the same probe.
  Regular cadences up to 20 ms can extend the nominal 10 ms target to two wakes
  plus 2 ms for usable timestamps, within the existing byte reserve. Longer
  stalls never increase retained cadence. Originals and repairs consume the
  same probe rate, and actual transmission times drive feedback.

  Every screen-watch transport dispatches clusters at least one second apart,
  including startup pairs and fast active viewers. Target increases wait for a
  full reserve within one MTU, and background padding stops at its source while
  it refills. This prevents reserve exhaustion from manufacturing TWCC loss.

  A fully bandwidth-paused viewer restarts ordinary startup probing from the
  same 2.5 Mbps estimate as a fresh subscriber, as soon as the reserve is ready.
  If no paused lane fits, it retries after 15 s once the reserve refills. A
  continuous-fit guard in progress finishes before another reset, and ready
  priority handoffs/resumes run first. There is no custom recovery probe,
  estimate-progress tracking or adaptive target backoff. Resets discard
  congestion/ACK/loss history, estimate smoothing,
  in-flight TWCC send records, pending padding from both the fallback and every
  media/RTX source, pacer debt and scheduler history. Queued media and repair
  caches survive the reset. RTP, TWCC and SRTP sequence counters and the
  transport stay intact.
  Existing socket credit is preserved, never replenished by a reset. Both
  desired-bitrate copies are clamped to 1.5 times the new estimate before reset.
  Once the normal ten-second continuous-fit guard permits resume, sparse
  pause-time feedback is discarded again at the measured capacity, so active
  video starts with fresh measurements.

  Capacity rediscovery also fills the first probe burst at the right clock.
  Released media can start an unpaced cluster before padding is requested.
  The first padding request now tops up that same burst, counting media already
  sent, instead of waiting another scheduler wake and sending a large late tail.
  That distorted burst pattern could undermeasure a healthy link and cause a
  false re-pause after a successive or long outage. Byte-limited bursts still
  leave a tail for a later timestamp, including when media starts the cluster.
  Paced media retains its original timing and first-burst size.

  Released media can also consume that reserved tail at the first timestamp.
  Once their byte and packet targets are met, byte-limited probes remain open
  for a later timestamp within the supported regular cadence (at most 20 ms).
  If no media arrives then, one blank packet uses the existing MTU rounding
  reserve. A longer scheduler stall ends that single-timestamp cluster without
  tagging another packet; its invalid result is discarded, so scheduling delay
  cannot become a false low-capacity sample. Without this guard, the estimator rejected zero-duration clusters,
  leaving a healthy 20 Mbps viewer at about 7.75 Mbps estimated capacity and
  pausing it at 15.06 s with 1% loss and 15 ms scheduling. The separate
  outage-free regression requires zero pauses for every cadence from 1 through
  20 ms at 0%, 1% and 2% loss (60 cases), plus at least 7.2 Mbps received in
  each of the final three ten-second windows. Neither allocator thresholds nor
  upstream's unbounded-probe timing changes.

  This allocator-driven pause does not exist on current `main` (`a78f775`):
  source comparison confirms it has no screen bandwidth allocator and its watch
  transports do not enable BWE. This is a source-based comparison; the PR's
  simulation cannot run unchanged there because that egress path is absent.

  Permanent regressions include the reviewer's exact 15 ms cadence with a
  100 ms stall every 2 s and a 12-42 s collapse, and the successive 12-42 s
  and 222-251 s collapses with 1% loss. Parameterized cases cover 1/10/15/20 ms
  cadences, 60/100/150/250 ms stalls every 1.3/2/3.5/5 s, one to three outages,
  and 0-2% loss. Every restored interval requires resume within 180 s, no
  re-pause, and at least 7.2 Mbps received in each of its final three ten-second
  windows. The existing 70-case matrix keeps the same assertions.

  The originals release and socket buckets retain 10 ms of credit at normal
  cadences and allow 30 ms when measured wakes exceed 10 ms. This covers the
  tested 20 ms cadence plus estimate-change margin; their rate remains
  `0.8 × estimate`. The fixed 10 ms capacity discarded half the pacing credit
  between 20 ms wakes and repeatedly starved resumed video. The extras rate,
  socket cap and bucket capacity, and every pause/resume threshold remain
  unchanged. Release and socket credit refill under the old burst window before
  its size changes, so released media cannot spend credit the socket clipped.
  Pacer snapshots refresh immediately after writing released originals.

  The supported resume rule is **10 s of continuous measured room**.
  Capacity rediscovery after physical restoration adds a delay that is not
  bounded in general; neither 10 s nor the previous 45 s claim is a general
  recovery deadline. The earlier regression missed 28/29 s collapses, slow
  15/20 ms cadences and downlink loss.

  The parameterized regression
  (`game_recovers_after_different_collapse_durations`) runs an 8 Mbps/60 fps
  Game share with a 200 KB keyframe. A 20 Mbps link collapses to 1 Mbps at
  12 s for 15, 28, 29, 30, 30.1, 60 or 120 s. Each duration runs at 1, 5, 10,
  15 and 20 ms SFU cadences, on a lossless link and with 1 % downlink loss
  throughout: 70 cases. All observe another 360 s after restoration,
  require resume within a generous 180 s test bound, forbid subsequent
  pauses, and require at least 90 % of the intended payload rate in each
  of the final three ten-second receiver windows. This is a deterministic
  regression bound for these fixtures, not a production recovery guarantee.
  Every case must have zero startup pauses and send originals before the
  outage, then meet the same recovery assertions.
  The separate scheduling-restoration case changes 10 ms back to 1 ms
  40 s after restoration and applies the same resume and throughput checks.

  These tables were regenerated from the complete 70-case regression with the
  timestamp guard and complete padding-reset fixes above.

  **Lossless: resume delay in seconds after physical restoration.**

  | Collapse | 1 ms | 5 ms | 10 ms | 15 ms | 20 ms |
  | --- | ---: | ---: | ---: | ---: | ---: |
  | 15 s | 28.644 | 15.845 | 14.550 | 54.735 | 33.960 |
  | 28 s | 15.644 | 18.645 | 16.650 | 41.735 | 20.960 |
  | 29 s | 14.644 | 17.645 | 15.550 | 40.735 | 19.960 |
  | 30 s | 13.644 | 16.645 | 14.550 | 39.735 | 18.960 |
  | 30.1 s | 13.544 | 16.545 | 14.450 | 39.635 | 18.860 |
  | 60 s | 13.644 | 18.245 | 14.550 | 28.530 | 24.760 |
  | 120 s | 39.644 | 21.445 | 14.550 | 19.035 | 18.460 |

  **1 % downlink loss: resume delay in seconds after physical restoration.**

  | Collapse | 1 ms | 5 ms | 10 ms | 15 ms | 20 ms |
  | --- | ---: | ---: | ---: | ---: | ---: |
  | 15 s | 28.844 | 15.645 | 15.550 | 39.090 | 32.460 |
  | 28 s | 15.844 | 17.545 | 16.650 | 26.090 | 19.460 |
  | 29 s | 14.844 | 16.545 | 15.550 | 25.090 | 18.460 |
  | 30 s | 13.844 | 15.545 | 14.550 | 24.090 | 17.460 |
  | 30.1 s | 13.744 | 15.445 | 14.450 | 23.990 | 17.360 |
  | 60 s | 13.844 | 18.045 | 15.550 | 30.000 | 24.760 |
  | 120 s | 40.844 | 21.445 | 15.550 | 22.080 | 19.660 |


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
  short clusters normally capped at 1.2× estimate and paced together with media,
  within the viewer's extras bucket ("Per-viewer egress cap").

  Removing the final assignment sets the desired bitrate to zero immediately.
  The socket suppresses any pending non-control datagrams while nothing is
  watched, including probe clusters already scheduled by str0m. A retained
  transport sends zero non-control bytes in the 30 s final-unwatch simulation.

  The desired target is **at most 1.5 × the current estimate**; the actual
  default probe ceiling is **1.2 × estimate**, raised only for the priority
  share's demand and bounded at 2×. Both the estimator's probe plan and
  its continuation threshold use the capped rate. A successful capped probe
  can therefore continue discovering capacity, without waiting for a result
  at the original higher target. The already-probed maximum also includes this
  ceiling: reaching it stops exponential retries at the same rate instead of
  manufacturing a new cluster on every one-second result timeout. Periodic
  discovery and a raised ceiling remain available. The existing extras byte reserve also bounds
  coarse-clock clusters. Target increases wait for reserve refill within one
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
  `K ÷ B` plus 30 ms.
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

The native byte layouts are implemented in `crates/gwar-media/src/sframe.rs`
and use the same RFC 9605 `AES_128_GCM_SHA256_128` suite (0x0004) as voice.
`H` below is a minimal RFC 9605 header (1–17 bytes), `C` is ciphertext of the
same length as its plaintext, and `T` is the 16-byte AES-GCM tag. RTP payload
descriptors are outside these layouts.

```text
VP9 key or delta:   H | C(entire encoded frame) | T
                   AAD = H
VP8 key (bit 0=0):  P[10] | H | C(frame[10..]) | T
VP8 delta (bit 0=1):P[3]  | H | C(frame[3..])  | T
                   AAD = H | P
```

- VP8 uses bit 0 of the first encoded byte (the frame tag) to choose the
  prefix length on both seal and open. The prefix includes the frame tag
  and, on keyframes, the sync code and dimensions. It is RFC 9605 metadata:
  **all clear bytes are authenticated**, including that frame-type bit.
  Fewer than 10 keyframe bytes or 3 delta bytes are rejected. The split
  preserves bytes exactly; it does not validate compressed video syntax.
  Prefix-only VP8 and empty VP9 have valid byte layouts (zero ciphertext
  bytes plus a tag), independently of whether a decoder accepts them.
- Header integers use the config byte's inline form for values 0–7, or
  minimal big-endian extended KID/CTR values of 1–8 bytes each. Extended
  values below 8 and leading-zero encodings are rejected, even with a valid
  authentication tag; receivers never rewrite a header before opening it.
  There is no additional VP9/VP8 canonicalization or escaping.
- Keys are `MLS-Exporter("gwar screen v1", u32be(leaf), 32)` from the
  accepted full epoch, with `KID = (epoch mod 2^32) << 32 | leaf`.
  The label is not a wire field or an additional SFrame KDF step.
  Native `Agent::screen_keys` applies the same live membership/credential
  gates as voice. A separate `SframeState::new_screen` owns screen keys,
  counters and replay windows; changing VP9/VP8 must reuse that instance.
  Production sending retains voice's limit of 2^32 attempts per base key;
  the low-level layout vectors also exercise all RFC counter widths.
- Bounds are on the **encoded plaintext**, at most 2 MiB, checked before
  encryption/allocation. The wire bound is 2 MiB + 33 bytes (widest header
  and tag); opening also checks the recovered plaintext length from the
  actual header width before authentication. This is separate from the
  SFU's smaller per-frame ingress limits.
- Shared vectors: `crates/gwar-media/tests/screen-sframe-vectors.json`.
  Rust and `apps/web/src/voice/sframe/screen-vectors.test.ts` both seal and
  open the same bytes, using the existing Rust SFrame library and unchanged
  TypeScript `suite.ts`, respectively. The independent WebCrypto generator
  is beside the JSON file. At base revision `4edc38ad`, the web worker is
  **voice-only** and screen transports attach no encryption transform;
  these tests establish primitive/layout compatibility, not deployed screen
  worker or browser packetizer interoperability. Screen-worker integration
  remains open. H.264/AV1 layouts remain open with E2/A2/A2b.

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
| `screen.publish.offer` | `{share, ssrc, rtx_ssrc, sdp}` | `{sdp}`; the share's own publish transport. Its first connection announces `screen.started` to the channel. One per share, reopened only by bounded SFU `screen.recover` |
| `screen.stop` | `{share}` | ok; event `screen.stopped {share, reason: "stopped"}` |
| `screen.watch.offer` | `{run, sdp, browser?, h264?}` | `{sdp}`; the session's watch transport (4 `recvonly` slots). `run` restarts it like voice's run; a run is retired for good once offered, so a failed or closed run and every lower one answer `stale` |
| `screen.watch` | `{share}` | `{slot}`; needs a connected watch transport (`unavailable` otherwise); the server assigns a slot, opens a segment at the next keyframe (throttled request), event `screen.slot` |
| `screen.focus` | `{share}` | ok; the share on the viewer's stage gets first claim on its bandwidth |
| `screen.unwatch` | `{share}` | ok; the slot is freed |
| `screen.keyframe` | `{share}` | ok; throttled PLI. Lets a viewer whose decryptor lost sync, for example after a key change, recover without re-watching |
| `screen.stop_other` | `{share}` | needs `mute_members` in the channel and rank above the sharer; event `screen.stopped {reason: "moderator", by}` |
| `screen.link.revoke` | `{share}` | the sharer, or `mute_members` with rank above the sharer; the share's watch link stops working and its guests are disconnected; final for that share |

The optional `screen.capabilities {}` query replies `{screen_h264: true}`; the
`guest.watch` reply's `screen_h264` boolean advertises the server's A2 capability
and recovery support to all guests. Legacy guest response validation ignores
this extra field and continues with the legacy `guest.offer {sdp}`. Clients omit `browser`,
`h264` and decode reports unless the flag is true. An unsupported/timed-out query selects legacy fields.
Servers retain the legacy Welcome shape and accept old clients
that omit all new fields.

Events:

- `screen.started {share, client, channel, preset, surface}`;
- `screen.viewers {share, members, guests}`, to the sharer only, and
  `screen.link {share, active}` to the sharer and, once the share is announced, the channel's moderators;
- `screen.stopped {share, reason: stopped|moderator|left|limit|transport|budget|access|codec}`;
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
  - After that, only `guest.offer {sdp, browser?, h264?}` is accepted, **once per
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
- **`screen.stopped {reason: "codec"}`** ends only my incompatible watch,
  preserves the listed share and watch link, and displays a decoding error.
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
     (H.264/VP9 + RTX), polling after every input. A share is announced when
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
   retrying fresh startup after 15 s with no lane fitting, Retry resuming bandwidth pauses, and the screen
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
11. **H.264 preferred for browser sending (A2/E1 landed):**
    prefer H.264 for every preset, hardware where available
    (`powerEfficientEncoder` in stats), VP9 only without an H.264 encoder,
    and remove VP8. Encrypted H.264 also needs B2/E2's NAL-aware SFrame with
    start-code escaping. Browser H.264 precedes A2b's AV1 + DD work.
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
  drops to 720p60 when `qualityLimitationReason` is `cpu`. A2/E1 prefers
  H.264 hardware for all presets and reserves VP9 for emergency fallback.
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
