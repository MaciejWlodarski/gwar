# Native clients and native media (plan)

Status: **plan, not scheduled.** Nothing here has a date. It describes where
the clients are going, which pieces exist, and which pieces a contributor can
build, in what order, and how each one is accepted.

It extends [screen-share.md](screen-share.md). That design ships screen
sharing through the server's SFU from browsers and the Tauri desktop app's
webview. This plan covers what comes after: native capture, encoding and
watching in Rust and per-platform adapters, on every platform. Where it
changes an earlier plan, it says so ([Changes to earlier plans](#changes-to-earlier-plans)).

Facts about platform APIs link to their source. Anything not confirmed in
official documentation is marked **(to verify)**. The list is collected in
[Items to verify](#items-to-verify).

## Contents

1. [Goal and scope](#1-goal-and-scope)
2. [Architecture target](#2-architecture-target)
3. [Codecs](#3-codecs)
4. [Resolutions and quality](#4-resolutions-and-quality)
5. [Capture per platform](#5-capture-per-platform)
6. [Watching natively](#6-watching-natively)
7. [Shared audio](#7-shared-audio)
8. [Work breakdown](#8-work-breakdown)
9. [Testing strategy](#9-testing-strategy)
10. [Risks and open questions](#10-risks-and-open-questions)

## 1. Goal and scope

### Where we are

| Client | UI | Voice | Screen sharing |
| --- | --- | --- | --- |
| Web (`apps/web`) | React | WebRTC in the browser, SFrame worker | Being built: `getDisplayMedia`, VP9/VP8 ([screen-share.md](screen-share.md)) |
| Desktop (`apps/desktop`, Tauri 2) | the same React UI in the system webview | **native**: `gwar-client` (str0m, cpal, Opus, SFrame in Rust) | macOS 14+ and Windows through the webview; none on Linux ([owner decision 1](screen-share.md#owner-decisions)) |
| Android | being built separately (docs on its branch) | | |
| iOS | none | none | none |

A native Android client is being built separately (docs on its branch). It is
not on `main`.

### Goal

Eventually every client is native, desktop included. The web app stays as a
first-class client beside them. For media this means:

- **Native screen sharing on every platform:** macOS, Windows, Linux, Android
  and iOS. Capture, encoding, encryption and sending happen in native code,
  not in a webview.
- **H.264 with hardware encoding** wherever the platform has an encoder, and
  resolutions above today's 1080p cap (1440p, 2160p) where the network and
  the server allow it.
- **Screen plus audio sharing.** The share carries what the computer or phone
  plays.
- **Gwar's own audio is never in the share.** Other people's voices, played by
  Gwar, must not be captured. Otherwise viewers hear themselves, or the call,
  echoed back.
- **Native watching:** hardware decoding and rendering of shares, and playback
  of shared audio mixed with voice.

### Non-goals

- Changing the wire protocol or the SFU's model. Native clients are ordinary
  WebRTC peers on the same `screen.publish.offer` and `screen.watch.offer`
  transports ([§2](#wire-compatibility)).
- Transcoding on the server. The SFU forwards; it never decodes or re-encodes
  ([screen-share.md §3](screen-share.md#3-the-sfu)).
- Camera video, recording, remote control and peer-to-peer. They stay where
  [screen-share.md](screen-share.md#goals-and-non-goals) put them.
  Peer-to-peer is still that document's phase 3.
- Game capture by injection (hooking a game's swap chain, as OBS's game
  capture does). Gwar uses only the operating system's capture APIs.
- Dropping the web app or the Tauri desktop app before native clients replace
  them. The Tauri app stays the desktop client until a native desktop UI
  exists, and it is the first consumer of the native media engine
  ([B10](#track-b-shared-rust-media-core)).
- Choosing the native desktop UI toolkit. That is an
  [open owner decision](#owner-decisions-needed).

## 2. Architecture target

```mermaid
flowchart LR
  subgraph UI [Native UI per platform]
    K[Kotlin / Android]
    S[Swift / iOS, macOS]
    W[Windows UI]
    L[Linux UI]
    T[Tauri app, until native desktop UIs exist]
  end
  UI -- UniFFI or Tauri commands --> CORE
  subgraph CORE [Shared Rust media core]
    TR[Transports: str0m<br/>voice, publish, watch]
    SF[SFrame: voice, video, share audio]
    PK[Packetizers, pacing, BWE, stats]
    RC[Rate and quality control]
  end
  subgraph AD [Platform adapters behind traits]
    CAP[ScreenSource / AudioSource]
    ENC[VideoEncoder]
    DEC[VideoDecoder / VideoSink]
  end
  CORE --- AD
  TR -- WebRTC, same protocol --> SFU[gwar-server SFU]
  WEB[Web app, unchanged] -- WebRTC --> SFU
```

### One media core

Today `crates/gwar-client` holds the native voice engine: str0m in RTP mode,
cpal, Opus, the jitter buffer, echo cancellation and RFC 9605 SFrame
([voice-native-5d.md](voice-native-5d.md)). Native media grows from the same
place. The target is one engine for voice and screen:

- **Transport.** str0m 0.24, as voice. Screen transports use str0m's
  **frame mode**: str0m packetizes outgoing frames and depacketizes incoming
  ones (VP8, VP9, H.264, H.265 and AV1 packetizers are in
  `str0m-0.24.1/src/packet/`). Each share is its own `Rtc`, exactly as a
  browser's is ([screen-share.md, Transports](screen-share.md#transports)).
- **SFrame.** The key state that 5d ported to Rust
  (`crates/gwar-client/src/voice/sframe.rs`) is reused per media kind, with
  the video frame layouts from
  [screen-share.md, Frame layout](screen-share.md#frame-layout) and the H.264
  layout from [§3](#encryption-nal-aware-sframe).
- **Pacing and bandwidth.** str0m's send-side estimate (TWCC) on the publish
  transport drives the encoder's target bitrate. str0m's pacer sends.
- **Stats.** One counters type per transport, exported like
  `voice_crypto_counters`, and readable from every UI.
- **Threads.** The voice pipeline keeps its dedicated real-time thread
  ([latency.md, Pipeline scheduling](latency.md#pipeline-scheduling)) and the
  voice RTC task stays separate. Screen transports, encoders and decoders run
  on their own threads and never block either.

The proposed layout is a new crate, `crates/gwar-media`, for the
platform-neutral parts (traits, frame types, SFrame layouts, transports, rate
control), and platform adapters as modules or crates behind `cfg`:

| Adapter set | Platforms | Contents |
| --- | --- | --- |
| `apple` | macOS, iOS | ScreenCaptureKit, Core Audio taps, VideoToolbox |
| `windows` | Windows | Windows.Graphics.Capture, WASAPI, Media Foundation |
| `linux` | Linux | xdg-desktop-portal, PipeWire, VA-API |
| `android` | Android | the Rust side of MediaCodec and MediaProjection hand-off |
| `soft` | all | openh264, libvpx (software encode and decode) |

Voice moves into `gwar-media` only if that makes sharing code simpler. Moving
it is not a goal, and the voice engine's latency gates apply to any change
that touches it.

### Adapter traits

The traits are sketches. Their final form is work item
[B1](#track-b-shared-rust-media-core).

```rust
/// A screen, window or app picked by the user through the platform picker.
pub trait ScreenSource: Send {
    fn caps(&self) -> SourceCaps;                 // max size, fps, cursor modes, HDR
    fn start(&mut self, cfg: CaptureConfig, sink: Box<dyn FrameSink>) -> Result<()>;
    fn reconfigure(&mut self, cfg: CaptureConfig) -> Result<()>; // size and fps caps
    fn stop(&mut self);
}

/// System or application audio, with Gwar's own output excluded.
pub trait AudioSource: Send {
    fn exclusion(&self) -> Exclusion;             // OwnProcessExcluded | NotExcluded
    fn start(&mut self, sink: Box<dyn AudioSink>) -> Result<()>; // 48 kHz stereo f32
    fn stop(&mut self);
}

pub trait VideoEncoder: Send {
    fn caps(&self) -> EncoderCaps;                // codec, profiles, max size, hardware
    fn encode(&mut self, frame: RawFrame, keyframe: bool) -> Result<()>;
    fn set_rate(&mut self, bitrate_bps: u32, max_fps: u32);
    // Output: EncodedFrame { codec, data (Annex B for H.264), keyframe, capture_ts }
}

pub trait VideoDecoder: Send {
    fn decode(&mut self, frame: EncodedFrame) -> Result<()>; // output to a VideoSink
    fn reset(&mut self);
}

pub trait VideoSink: Send {
    fn present(&mut self, frame: DecodedFrame);  // GPU surface where possible
}
```

`RawFrame` and `DecodedFrame` carry a platform GPU surface (an `IOSurface`, a
D3D11 texture, a dmabuf, an Android `Surface`) when the adapter can stay on
the GPU, and CPU planes (NV12, I420, BGRA) otherwise. The zero-copy path is
the normal one on every platform with a hardware encoder.

Two integration styles are allowed:

- **Rust-owned adapters** (desktop). Rust calls the OS APIs through `objc2`,
  `windows`, `ashpd` and `pipewire` crates, and owns the whole pipeline.
- **Platform-owned capture and encode** (mobile). On Android, MediaProjection
  renders into a MediaCodec input `Surface` with no copy. On iOS, the
  capture callback hands sample buffers to VideoToolbox. The platform code
  pushes `EncodedFrame`s into Rust through an `EncodedVideoSink` interface.
  Rust still owns SFrame, packetization, transport and rate control, and
  tells the platform encoder its target bitrate and when to make a keyframe.

### The UniFFI boundary

Native UIs call Rust through [UniFFI](https://mozilla.github.io/uniffi-rs/),
which generates Kotlin and Swift bindings. Shared logic stays in Rust. The
boundary is coarse:

- objects: `MediaSession` (per connection), `ShareHandle`, `WatchHandle`;
- calls: start or stop a share with a preset and a source, watch or unwatch
  a share, set its volume, read stats;
- callback interfaces: share and watch state, stats, errors, and, for
  platform-owned encoders, `EncodedVideoSink` (frames in) and
  `EncoderControl` (bitrate and keyframe requests out).

Per-frame data does not cross the boundary as copies where it can be avoided.
GPU surfaces and native windows cross as opaque handles through a thin
platform shim (JNI for an Android `Surface`, a pointer for a `CALayer`),
because UniFFI has no type for them **(to verify: current UniFFI support for
foreign handles)**. Windows and Linux bindings depend on the desktop UI
decision ([Owner decisions needed](#owner-decisions-needed)). A Rust UI on
Linux would need no bindings at all.

The Tauri app is the exception: it reaches the same core through Tauri
commands, like voice today. It keeps the command-surface rules of
[architecture.md, Native command surface](architecture.md#native-command-surface).

### The web client stays first-class

- The browser keeps capturing with `getDisplayMedia` and sending from the web
  layer, as designed in [screen-share.md](screen-share.md).
- It needs **no server change** for native peers. A native sharer is just
  another publisher; a native viewer is just another watch transport.
- The only web changes are on the receiving side: decoding H.264 shares from
  native sharers, and handling a codec switch
  ([Track E](#track-e-web-side-changes)).

### Wire compatibility

- Same protocol: `screen.start`, `screen.publish.offer`,
  `screen.watch.offer`, `screen.watch`, `screen.keyframe` and the events in
  [screen-share.md, Requests](screen-share.md#requests-gwar4). A native
  client sends the same SDP shapes a browser sends: one `sendonly` video
  m-line with RTX per share, four `recvonly` video m-lines per watch
  transport, and the audio m-lines of
  [Shared audio](screen-share.md#shared-audio-phase-2) once they exist.
- Same SFU, same admission, same limits, same keyframe throttle. The SFU
  cannot tell a native peer from a browser, and must not need to.
- Same keys: `MLS-Exporter("gwar screen v1", …)` for video and
  `"gwar screen audio v1"` for share audio, with the same frame layouts.
- The protocol additions this plan needs (H.264 and codec switching,
  resolution tiers, simulcast) are additions every client gets, the web app
  included. They are in [Track A](#track-a-sfu-and-protocol).

## 3. Codecs

### What each side does today

| | VP9 | VP8 | H.264 | AV1 | H.265 |
| --- | --- | --- | --- | --- | --- |
| Web sharers (phase 1) | preferred | fallback | not sent | no | no |
| SFU (PR 3a, #136) | forwarded | forwarded | **not negotiated** | no | no |
| str0m 0.24.1 | packetizer, depacketizer | yes | yes (STAP-A out, FU-A) | yes | yes |
| Browsers decode | all | all | all, see below | most | some |
| Hardware encoders | some Intel/NVIDIA, none on Apple | rare | **everywhere** | newest GPUs | Apple, recent GPUs |

Sources: [screen-share.md, Support](screen-share.md#support), the str0m
source.

### H.264 plan

H.264 is the codec with hardware encoders on every target platform. It is the
default for native sharers once the SFU and the web viewers handle it.

#### SFU payload handling (A1, A2)

The SFU reads only enough of each RTP payload to find frame and keyframe
starts, as it does for VP9 and VP8 (`describe` in
`media/screen/ingress.rs`, PR 3a). For H.264
([RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)):

- The NAL unit type is the low 5 bits of the first payload byte.
  - 1–23: a single NAL unit packet.
  - 24 (STAP-A): aggregated NAL units, each with a 16-bit size. The SFU walks
    them, bounded by the payload length.
  - 28 (FU-A): a fragment. The FU header's S bit marks the first fragment, and
    its type field is the fragmented NAL's type.
  - Other types (STAP-B, MTAP, FU-B) are refused: they are not allowed in
    `packetization-mode=1`.
- **Frame start:** the first packet of a new RTP timestamp, as for VP8/VP9.
- **Keyframe start:** the first packet of an access unit whose first NAL
  (single, first STAP-A unit, or FU-A with S = 1) is an SPS (7), a PPS (8) or
  an IDR slice (5).
- **The access unit must contain an IDR.** The SFU forwards per packet and
  does not assemble frames, so it decides at the first packet and checks by
  the end of the access unit. A segment opened at an SPS whose access unit
  ends (marker bit or new timestamp) without an IDR slice is treated as a
  failed keyframe: the rest is dropped and one throttled PLI is sent
  ([Keyframes](screen-share.md#keyframes)).
- **Publisher contract:** every IDR is preceded by SPS and PPS in the same
  access unit. str0m's packetizer does this when the encoder emits them
  (it sends a STAP-A of SPS and PPS before the next NAL). Native encoders are
  configured to repeat parameter sets on every IDR (VideoToolbox does by
  default; MediaCodec has `KEY_PREPEND_HEADER_TO_SYNC_FRAMES`, API 29). Chrome's
  and Safari's behavior is checked by E1 **(to verify)**. The SFU never
  caches or injects parameter sets.
- Malformed payloads (a STAP-A unit longer than the packet, an FU-A with both
  S and E set, an empty NAL) are dropped and counted, like a truncated VP9
  descriptor.

#### Negotiation (A2)


- `packetization-mode=1` only. Mode 0 (one NAL per packet) cannot carry a
  1080p slice in one MTU and is not offered.
- **Profiles:** Constrained Baseline (`42e0xx`) is accepted everywhere.
  Constrained High (`640cxx`) is accepted when the viewer offered it. Main
  and High with B-frames are not used: B-frames add a frame of latency.
- **Levels:** the publish answer sets `level-asymmetry-allowed=1`. The level
  in a viewer's offer is what it can receive. Browsers usually offer level
  3.1 (`42e01f`) and still decode 1080p in practice **(to verify per
  browser)**. The SFU therefore does not enforce levels; it matches profiles.
  Native viewers offer the level their decoder supports (5.1 for 2160p30).
- **Payload types:** one m-line can carry several codecs. The publish m-line
  offers every codec the sharer can encode (H.264 and VP9/VP8), and the SFU
  maps each viewer's payload type for the codec in flight.

#### Ingress frame bounds

H.264 frames are bounded like VP9 frames: 512 KB,
600 packets and 500 ms per RTP timestamp
([Ingress enforcement](screen-share.md#ingress-enforcement)). Higher
resolutions need tiered bounds ([§4](#sfu-limits-to-raise)).

#### Encryption: NAL-aware SFrame

Spike S3 showed browsers' H.264 packetizers send nothing when the whole frame
is encrypted ([screen-share.md, Spikes](screen-share.md#spikes)). The layout,
already chosen in [screen-share.md §2](screen-share.md#support), is:

- everything up to and including the first slice NAL's header stays clear
  (start codes, AUD, SPS, PPS, SEI and the slice NAL header byte); it is
  RFC 9605 *metadata*, so it is authenticated (AAD = `header || clear`);
- the rest, `SFrameHeader || AEAD(slice data and later NALs)`, is
  **start-code escaped**: any `00 00 0x` with `x ≤ 3` gets an emulation
  prevention byte, as in H.264 itself, so no packetizer splits the
  ciphertext. The receiver unescapes before opening.
- Without escaping, about one frame in 2^24 ciphertext bytes contains a
  start code and is lost. Spike S3's unescaped variant worked only by luck.

The exact byte rules (which NALs are clear when several slices exist, the
escaping, the bounds) are written once, with test vectors shared by Rust and
TypeScript, in work item [B2](#track-b-shared-rust-media-core)/[E2](#track-e-web-side-changes).
The same vectors gate both implementations, like
`crates/gwar-client/tests/sframe-interop.json` does for voice. SPS and PPS
stay clear, so the server sees resolution and profile; it already sees
resolution for VP9 ([What the server still sees](screen-share.md#what-the-server-still-sees)).

#### Browser interop

Native H.264 must play in every browser that watches.

| Viewer | H.264 decode | Notes |
| --- | --- | --- |
| Chrome / Edge | yes; hardware where available, FFmpeg otherwise | Chromium builds without proprietary codecs (some Linux distributions) may lack H.264 **(to verify)** |
| Safari / WKWebView | yes (VideoToolbox) | offers `42e01f` and `640c1f` (spike S1) |
| Firefox | yes, through the OpenH264 plugin Firefox downloads, or platform decoders | the plugin may be missing or disabled; High profile support **(to verify)** |
| Desktop app webview | as Safari (macOS) and Chrome (Windows) | Linux webview has no WebRTC |

This is why Constrained Baseline is the default and why a viewer that cannot
decode the share's codec must still be served ([codec negotiation](#codec-negotiation-across-mixed-viewers)).

### Hardware encoders per platform

| Platform | API | Notes |
| --- | --- | --- |
| macOS, iOS | VideoToolbox (`VTCompressionSession`) | `kVTVideoEncoderSpecification_EnableLowLatencyRateControl` (macOS 11.3+, iOS 14.5+) ([Apple](https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol)); takes `IOSurface`-backed pixel buffers from ScreenCaptureKit with no copy |
| Windows | Media Foundation H.264 encoder MFTs | vendor hardware MFTs (NVIDIA NVENC, AMD AMF, Intel Quick Sync) are found with `MFTEnumEx` and `MFT_ENUM_FLAG_HARDWARE`; D3D11 textures in through the DXGI device manager ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/medfound/h-264-video-encoder)); a direct NVENC path only if MFT latency falls short **(to verify)** |
| Linux | VA-API (Intel, AMD), NVENC (NVIDIA) | dmabuf from PipeWire into VA-API without a copy; some distributions build Mesa without H.264 encode for patent reasons **(to verify which)**; Vulkan Video encode is a later option |
| Android | MediaCodec | input `Surface` fed by the projection's virtual display; low-latency and priority keys vary by device **(to verify per vendor)** |

Encoder settings for screen content, all platforms: no B-frames, one slice
per frame unless the encoder needs more, infinite GOP with keyframes only on
request (PLI) or on a resolution change, CBR-like rate control with a short
window, and parameter sets on every IDR.

### Software fallbacks

Software encoding is the fallback when no hardware encoder exists, and the
reference for tests and CI.

- **H.264: OpenH264.** Cisco pays the MPEG LA royalties only for the binary
  module that Cisco distributes; "if a team uses the source code, the team is
  responsible for paying all applicable license fees", and products using the
  Cisco module "must download it at the time the product or project is
  installed" ([OpenH264 FAQ](https://www.openh264.org/faq.html)). The
  `openh264` Rust crate builds from source by default and can load Cisco's
  binary instead with its `libloading` feature
  ([openh264-rs](https://github.com/ralfbiedert/openh264-rs)). So:
  - release builds **download Cisco's binary** on first use and verify it,
    and never bundle a source-built OpenH264;
  - source builds are for tests and CI only;
  - whether downloading at first use rather than at install meets Cisco's
    terms is **(to verify)** and an [owner decision](#owner-decisions-needed).
- **VP9 and VP8: libvpx** (BSD), for software encode and decode. VP9 is the
  fallback codec of the mixed-viewer rule below, and every web sharer sends
  VP9 or VP8, so native viewers must decode both anyway.
- **FFmpeg is not used.** Its licensing (LGPL or GPL by configuration) and its
  patent exposure need a review the OS APIs and libvpx do not
  ([screen-share.md](screen-share.md#why-not-native-capture-now) noted the
  same).

### Later: AV1 and H.265

- **AV1** encodes screen content well (spike S3: 2.3 ms per 720p frame in
  Chromium), but hardware encoders are only on the newest GPUs, and the SFU
  would need the Dependency Descriptor extension for layers. Its SFrame needs
  an OBU-aware layout; spike S3 showed whole-frame encryption sends nothing.
- **H.265** has hardware encoders on Apple and recent GPUs, but Firefox does
  not decode it and Chrome only with hardware
  ([Support](screen-share.md#support)).
- Neither is planned. Each would be one more `describe` and one more SFrame
  layout, behind the same negotiation rules.

### Codec negotiation across mixed viewers

The SFU forwards and never transcodes, so every viewer must decode what the
sharer sends. The rule ([A3](#track-a-sfu-and-protocol)):

1. **Viewers declare what they decode.** A watch offer's m-lines list the
   codecs the viewer can receive. The SFU records the set per watch
   transport. Today's browsers all list VP9 and VP8.
2. **Sharers declare what they encode.** The publish m-line lists every codec
   the sharer can send, preferred first. A native sharer offers
   `[H.264, VP9, VP8]` when it has a hardware H.264 encoder, `[VP9, VP8,
   H.264]` otherwise. Web sharers keep today's `[VP9, VP8]`.
3. **The sharer starts with its preferred codec.** No negotiation round
   happens before the first frame.
4. **A viewer that cannot decode the current codec is not refused.** The SFU
   assigns it a slot that waits, and asks the sharer to switch
   (`screen.codec {share, codec}`, a new event to the sharer). The codec asked
   for is the best one in the intersection of what the sharer encodes and
   what every current viewer of the share decodes.
5. **The sharer switches at a keyframe**, on the same m-line (a payload type
   change; no renegotiation). Each slot's segment starts at the first
   keyframe in a codec its viewer decodes, using the existing
   wait-for-keyframe rule.
6. **Switching back** to the preferred codec happens when the last viewer
   that needed the fallback leaves, after a 30 s hold so a viewer flapping in
   and out cannot make the sharer flap.
7. **The fallback may lower quality.** A sharer that switches from hardware
   H.264 to software VP9 also lowers the resolution tier to what its CPU can
   encode ([§4](#cpu-and-gpu-budgets)).
8. **If the intersection is empty** (a viewer that decodes nothing the sharer
   encodes), the watch fails with `screen.stopped {reason: "codec"}` and the
   UI says the viewer's browser cannot play this share. This is not expected
   with VP8 in every list, but must be handled.

An alternative is to refuse viewers that cannot decode H.264 and never
switch. It is simpler, and it locks out Chromium builds without H.264 and
Firefox without its plugin. The choice is an
[owner decision](#owner-decisions-needed); this plan recommends switching.

## 4. Resolutions and quality

### Presets beyond 1080p

Today's presets cap at 1080p ([Presets](screen-share.md#presets)). This plan
adds **resolution tiers** to the existing presets instead of new preset
names. A tier is the resolution cap; the preset still decides frame rate,
content hint and how to degrade.

Starting points for H.264 hardware encoding. They are to be measured
([F3](#track-f-measurements-and-qa)) before any default changes.

| Preset | 1080p (today) | 1440p | 2160p |
| --- | --- | --- | --- |
| Text (15 fps, keep resolution) | 2.5 Mbps | 3.5 Mbps | 6 Mbps |
| Smooth (30 fps) | 4 Mbps | 7 Mbps | 12 Mbps |
| Game (60 fps, keep frame rate) | 8 Mbps | 14 Mbps | 2160p30 only: 20 Mbps |

- 2160p60 is not planned: 30 Mbps and more per viewer, and few encoders do
  it at low latency.
- **Text at 1440p and 2160p fits today's 8 Mbps per-share cap.** That is the
  case that benefits most (code and documents on high-DPI screens). Smooth
  1440p fits too. Game 1440p and anything at 2160p beyond Text need a higher
  cap.
- The per-share cap and the 300 Mbps server budget are
  [owner decisions](screen-share.md#owner-decisions) (decision 4). This plan
  **does not change the defaults.** Tiers above what the server's cap allows
  are not offered. Raising the defaults is an
  [open owner decision](#owner-decisions-needed).
- VP9 needs roughly the bitrates above at equal quality **(to verify; F3)**;
  its software encoder is the limit at 1440p60.

### SFU limits to raise

The SFU's per-share limits were sized for 1080p
([Admission and resource limits](screen-share.md#admission-and-resource-limits)).
Higher tiers need them scaled per share, from the share's tier and cap
([A4](#track-a-sfu-and-protocol)). Proposed values, to be confirmed by F3:

| Limit | Today | 1440p tier | 2160p tier |
| --- | --- | --- | --- |
| Frame bytes per RTP timestamp | 512 KB | 1 MiB | 2 MiB |
| Frame packets | 600 | 1,100 | 2,000 |
| Byte bucket burst | 512 KB | 1 MiB | 2 MiB |
| Packet rate | 2,000/s | `max(2,000, 1.25 × cap ÷ 9,200 bits)` | same formula |
| Transport meter | 2,500 packets/s, 1.5 × cap | scaled with the packet rate | same |
| RTX cache per stream | 1,024 packets | ≥ 2 × the frame packet bound | same |
| Encrypted frame bound | 2 MiB | 2 MiB | 3 MiB (escaping can grow ciphertext) |

- The frame span bound (500 ms) and the three-overruns rule stay.
- The RTX cache must hold a whole keyframe, or the first packets of a large
  keyframe cannot be repaired.
- **The 400 ms original-packet deadline does not scale.** A keyframe must
  reach a viewer within 400 ms at `0.8 × estimate`
  ([Queues and repairs](screen-share.md#queues-and-repairs)). A 1 MiB
  keyframe needs an estimate of about 26 Mbps, a 2 MiB one about 52 Mbps.
  Most viewers cannot watch a single-layer 2160p share. **So simulcast comes
  before 2160p is offered for anything but Text** ([next section](#simulcast-or-svc)).

### Simulcast or SVC

A single-layer share pauses viewers whose link cannot carry it
([Bandwidth allocation per viewer](screen-share.md#bandwidth-allocation-per-viewer)).
Layers let them watch at a lower quality instead.

| Option | Who sends it | SFU work | Fit |
| --- | --- | --- | --- |
| VP9 temporal layers (`L1T3`) | Chromium | drop by temporal id from the clear descriptor | screen-share PR 8; lowers fps only |
| VP9 SVC (`L2T3`, `L3T3_KEY`) | Chromium | spatial and temporal dropping from the descriptor | optional; no Firefox or Safari senders |
| **H.264 simulcast**, 2 layers | native sharers (two encoder sessions), Safari, Firefox | per-viewer layer choice, switch at keyframes | **the native plan** |
| VP8 simulcast | all browsers | picture id rewriting (`RtpWrite::vp8_patch`) | screen-share PR 13 |

The native plan is two H.264 layers: the share's tier, and a second layer at
720p and at most 30 fps. Notes:

- The publish m-line carries both layers by RID, as browsers do.
- The SFU picks a layer per viewer from the allocation of
  [Bandwidth allocation per viewer](screen-share.md#bandwidth-allocation-per-viewer),
  and switches only at a keyframe of the target layer, with a PLI for that
  layer alone. A layer switch is a new segment in the existing
  stable-sequence model ([Forwarding](screen-share.md#forwarding-stable-sequence-mapping)).
- Reservations charge the layer a viewer gets, not the top layer's cap.
- The sharer's uplink carries both layers, about 25 % more than the top layer
  alone.
- Hardware encoders limit concurrent sessions on some consumer GPUs
  **(to verify per vendor)**. A sharer that cannot open a second session
  sends one layer.
- This extends screen-share PR 13 (simulcast) to H.264 and native senders
  ([A5](#track-a-sfu-and-protocol)).

### Bandwidth and VM egress cost

Egress equals the forwarded rate per viewer
([Cost on the deployment VM](screen-share.md#cost-on-the-deployment-vm)).

| Share | Per viewer | 1 hour per viewer | Viewer-hours in 10 TB |
| --- | --- | --- | --- |
| Text 2160p | 1–6 Mbps (mostly static) | 0.5–2.7 GB | 3,700+ |
| Smooth 1440p | 4–7 Mbps | 1.8–3.2 GB | about 3,100 |
| Game 1440p60 | 10–14 Mbps | 4.5–6.3 GB | about 1,600 |
| Game 2160p30 | 15–20 Mbps | 6.8–9 GB | about 1,100 |
| 720p30 simulcast layer | 1.5–2.5 Mbps | 0.7–1.1 GB | about 9,000 |

- One 2160p30 share with 25 viewers at the top layer is 500 Mbps, more than
  the whole 300 Mbps budget. Simulcast puts most viewers on the 720p layer.
- The monthly egress counter (PR 3b) and the server budget stay the guards.
- SFU CPU scales with forwarded Mbps (about 0.4 % of an A1 core per Mbps,
  to be re-measured by PR 3b). 200 Mbps is about 0.8 of an A1 core.

### CPU and GPU budgets

Targets for a sharer, measured by [F2](#track-f-measurements-and-qa):

| Path | Target |
| --- | --- |
| Hardware encode, 1080p60 | ≤ 10 % of one CPU core for capture, encode submission, SFrame and packetization; encode ≤ 8 ms per frame |
| Hardware encode, 1440p60 or 2160p30 | ≤ 15 % of one core; encode ≤ 12 ms per frame |
| Software encode | 1080p30 at most; ≤ 1.5 cores |
| Voice | unaffected: the paired voice gate of [F4](#track-f-measurements-and-qa) |

When to downscale (work item [B6](#track-b-shared-rust-media-core)):

- The encoder reports late or dropped frames, or encode time stays above
  80 % of the frame interval for 2 s.
- The capture adapter reports dropped frames.
- The send-side estimate stays below the current tier's need.

Text keeps resolution and lowers frame rate (as `maintain-resolution`). Game
keeps frame rate and lowers resolution, one tier at a time (as
`maintain-framerate`). Recovery waits 10 s of headroom before stepping up,
to avoid oscillation. A hardware encoder's own silicon is separate from the
3D engine, so a game's frame rate should barely change; F2 measures it.

## 5. Capture per platform

Each platform below has two work items: video ([C1–C10](#track-c-per-platform-capture-and-encode))
and audio. The audio rule is the same everywhere: **if Gwar's own output
cannot be excluded, the share has no sound.** A system mix that contains
Gwar's voice playback is never sent. This is the same rule as
[Shared audio](screen-share.md#shared-audio-phase-2) for browsers.

### Summary

| Platform | Video API | Minimum OS (video) | Audio API, excluding Gwar | Minimum OS (audio) | Consent |
| --- | --- | --- | --- | --- | --- |
| macOS | ScreenCaptureKit + `SCContentSharingPicker` | 14 (owner decision 7; ScreenCaptureKit itself 12.3) | `SCStreamConfiguration.excludesCurrentProcessAudio`, or a Core Audio process tap | 13 / 14.2 | picker; Screen Recording (TCC) without it |
| Windows | Windows.Graphics.Capture | 10 1903 (HWND/monitor interop) | WASAPI process loopback, excluding Gwar's process tree | 10 build 20348 (in practice Windows 11) | picker, or none for interop capture |
| Linux | xdg-desktop-portal ScreenCast + PipeWire | portal v4+ for restore tokens | PipeWire: link every application's output except Gwar's to a capture node | PipeWire session | portal dialog; audio has no portal |
| Android | MediaProjection | 10 (API 29) for audio; 14 for single-app sharing | `AudioPlaybackCapture`; Gwar's voice uses `USAGE_VOICE_COMMUNICATION`, never capturable | 10 (API 29) | system dialog every session (14+) |
| iOS | ScreenCaptureKit (iOS 27+), ReplayKit broadcast extension before | 27, or 12 for the extension picker | ScreenCaptureKit audio with `excludesCurrentProcessAudio` (27+); app audio in the extension | 27 / 12 | system picker |
| Web | `getDisplayMedia` | as today | tab audio; `restrictOwnAudio` (Chrome 141+) | browser-specific | browser picker |

### macOS

**Video.**

- [ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit)
  is available from macOS 12.3. Frames arrive as `CMSampleBuffer`s backed by
  `IOSurface`, which go to VideoToolbox without a copy.
- [`SCContentSharingPicker`](https://developer.apple.com/documentation/screencapturekit/sccontentsharingpicker)
  (macOS 14+) is the system picker, and Apple's recommended way to choose
  content. Gwar uses it. That keeps the macOS 14 minimum of
  [owner decision 7](screen-share.md#owner-decisions), and matches what the
  webview path already uses.
- With the picker, no Screen Recording (TCC) permission prompt should be
  needed, because the picker is the consent **(to verify in a signed build;
  same open question as screen-share PR 1)**. Capturing without the picker
  needs the Screen Recording permission, which the user grants in System
  Settings. macOS 15 and later periodically ask the user to re-confirm apps
  that capture without the picker **(to verify the current cadence)**.
- `SCStreamConfiguration` sets size, frame interval (`minimumFrameInterval`),
  cursor and pixel format. HDR capture (`captureDynamicRange`) needs macOS 15
  ([Apple](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturedynamicrange)).
- **Keeping Gwar out of the picture.** A display filter can exclude
  applications' windows
  ([`SCContentFilter(display:excludingApplications:exceptingWindows:)`](https://developer.apple.com/documentation/screencapturekit/sccontentfilter/init(display:excludingapplications:exceptingwindows:))).
  Gwar excludes its own windows by default when a whole display is shared,
  so its chat and the stage are not broadcast and there is no hall of
  mirrors. A setting can turn this off. Whether a picker-chosen filter can be
  modified this way is **(to verify)**.

**Audio.**

- `SCStreamConfiguration.capturesAudio` (macOS 13+) adds system audio to the
  stream ([Apple](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/capturesaudio)).
- `excludesCurrentProcessAudio` (macOS 13+) leaves out audio from the
  capturing app ([Apple](https://developer.apple.com/documentation/screencapturekit/scstreamconfiguration/excludescurrentprocessaudio)).
  Gwar's native voice plays from the Gwar process through cpal, so it is
  excluded.
- **Caveat:** sounds the Tauri webview plays (notification sounds) come from
  WebKit's helper processes, which may not count as the current process
  **(to verify)**. The native app has no such helpers. Until verified, the
  Tauri app mutes webview sounds while sharing with audio.
- Alternative: a [Core Audio process tap](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps)
  (`AudioHardwareCreateProcessTap`, macOS 14.2+) captures a mix of chosen
  processes, or of all processes except chosen ones. It needs
  `NSAudioCaptureUsageDescription` and its own permission prompt. It is the
  fallback if ScreenCaptureKit audio proves unreliable, and it allows audio
  sharing without video.
- The microphone is separate. Gwar's voice keeps its own microphone path and
  permission. `captureMicrophone` (macOS 15) is not used.

### Windows

**Video.**

- **Windows.Graphics.Capture** is the main path
  ([Microsoft](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture)).
  Frames are D3D11 textures, which go to a hardware encoder MFT without a
  copy.
  - `GraphicsCapturePicker` is available from Windows 10 1803 (build 17134)
    ([Microsoft](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturepicker)).
  - `IGraphicsCaptureItemInterop::CreateForWindow` and `CreateForMonitor`
    create an item from an `HWND` or `HMONITOR` without the system picker,
    from Windows 10 1903 (build 18362)
    ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nn-windows-graphics-capture-interop-igraphicscaptureiteminterop)).
    Gwar can then show its own picker with thumbnails, like other desktop
    apps. **Minimum for native sharing: Windows 10 1903.**
  - **The yellow border.** The system draws a border around what is
    captured. `GraphicsCaptureSession.IsBorderRequired = false` removes it
    from Windows 10 build 20348, after the user consents through
    `GraphicsCaptureAccess.RequestAccessAsync(Borderless)`, which needs the
    `graphicsCaptureWithoutBorder` capability in a package manifest
    ([Microsoft](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired)).
    Whether an unpackaged app can declare it is **(to verify)**. Keeping the
    border is acceptable; it is also a privacy indicator.
  - `MinUpdateInterval` caps the capture rate on Windows 11 24H2 (build
    26100) and later
    ([Microsoft](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.minupdateinterval)).
    Before that, Gwar drops surplus frames itself (a 144 Hz screen delivers
    up to 144 frames a second).
  - HDR displays may deliver `R16G16B16A16_FLOAT`; Gwar converts to SDR on
    the GPU before encoding (the same Microsoft page).
- **DXGI Desktop Duplication** (whole monitors only) is a fallback for
  systems where Windows.Graphics.Capture is unsupported
  (`GraphicsCaptureSession.IsSupported()`)
  ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)).
  It is not planned unless F5's checklist finds such systems.
- **Keeping Gwar out of the picture.** `SetWindowDisplayAffinity(hwnd,
  WDA_EXCLUDEFROMCAPTURE)` hides a window from capture from Windows 10 2004
  ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)).
  As on macOS, it is the default while sharing a monitor, with a setting to
  turn it off.

**Audio.**

- **WASAPI process loopback**: `ActivateAudioInterfaceAsync` with
  `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` and
  `AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {TargetProcessId: Gwar's PID,
  ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE}`
  captures every render stream **except** those of Gwar's process and its
  children ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params),
  [sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-classic-samples/applicationloopbackaudio-sample/)).
  - The documented minimum is **Windows 10 build 20348**, which on clients
    means Windows 11. Reports say it works on Windows 10 2004 (build 19041)
    and later **(to verify)**.
  - In the Tauri app, WebView2's processes are children of the app, so they
    fall inside the excluded tree **(to verify)**.
- **Fallback:** whole-system loopback (`AUDCLNT_STREAMFLAGS_LOOPBACK` on the
  output device) ([Microsoft](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording))
  includes Gwar's own voice. Gwar does **not** offer it by default. Two
  options remain, both behind an explicit setting with a warning:
  - Gwar plays voice to a different output device than the one captured
    (for example headphones on another device);
  - no sound in the share (the default on Windows builds before process
    loopback works).

### Linux

Native capture is where the native path matters most. The Tauri app's
WebKitGTK has no WebRTC, so the Linux desktop app has no sharing today
([Desktop app](screen-share.md#desktop-app-tauri-212-wry-057)). This plan
does not reverse [owner decision 1](screen-share.md#owner-decisions) for
phase 1. It is the "native Linux path" that decision left open.

**Video.**

- The [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
  works on Wayland and X11. The user picks monitors or windows (source types
  1 and 2) in the desktop's dialog. `Start()` returns PipeWire streams;
  `OpenPipeWireRemote()` returns a file descriptor through which only those
  streams are visible.
  - Cursor modes: hidden, embedded or metadata (portal version 2+).
  - `persist_mode` and single-use restore tokens (version 4+) let a later
    share skip the dialog when the user allows it.
  - Version 6 deprecates the node id for targeting in favour of
    `pipewire-serial`.
- Frames arrive as dmabufs where the compositor supports it, and go to VA-API
  without a copy; otherwise as shared-memory buffers.
- Crates: `ashpd` (portal) and `pipewire` are mature
  ([Why not native capture now](screen-share.md#why-not-native-capture-now)).
- Encoding: VA-API or NVENC where available; otherwise OpenH264 (Cisco
  binary) or libvpx in software, at 1080p30 at most.

**Audio.**

- The ScreenCast portal has **no audio**. It defines only video source types
  (same page). An audio portal was proposed but has not landed **(to verify
  current status)**.
- Outside a sandbox, Gwar connects to PipeWire directly and can avoid
  capturing itself:
  - Gwar creates a private capture node (a virtual sink or an input stream).
  - It links the output ports of every application playback node
    (`media.class = Stream/Output/Audio`) **except its own** to that node,
    identifying its own nodes by `application.process.id` (and the PipeWire
    client's security PID), and follows nodes as they appear and disappear.
  - The original playback to the speakers is untouched; the links are added
    beside it.
  - [venmic](https://github.com/Vencord/venmic) (MPL-2.0) does this for
    Discord on Linux and is prior art for the approach and its edge cases.
- **Not** the default sink's monitor: that is the whole mix, Gwar included.
- **PulseAudio-only systems** (no PipeWire) get no shared audio. Their
  monitor sources contain Gwar's voice.
- **Flatpak:** the sandbox limits PipeWire access to what portals grant, so
  the per-application approach may not be possible in a Flatpak build
  **(to verify)**. Distribution format is an
  [owner decision](#owner-decisions-needed).

### Android

The Android client is being built separately (docs on its branch). Its media
items here plug into it later.

**Video.**

- [MediaProjection](https://developer.android.com/media/grow/media-projection):
  the app asks with `createScreenCaptureIntent()`, and the user consents in a
  system dialog.
  - **Consent every session from Android 14.** A projection token is
    single-use, and `createVirtualDisplay()` may be called once per
    projection; reuse throws `SecurityException`.
  - **Foreground service** of type `mediaProjection`, with
    `FOREGROUND_SERVICE_MEDIA_PROJECTION` for apps targeting Android 14.
  - **Single-app sharing** (Android 14+) is on by default: the user may share
    one app instead of the screen. Status bar, navigation bar and
    notifications are not in it.
  - Android 15 QPR1+ shows a status bar chip that stops sharing. Sharing also
    stops when the screen locks. The app must register
    `MediaProjection.Callback.onStop()`.
- The virtual display renders into a MediaCodec encoder's input `Surface`. No
  frame touches the CPU. Encoded output buffers go to Rust
  ([platform-owned encoders](#adapter-traits)).

**Audio.**

- [AudioPlaybackCapture](https://developer.android.com/media/platform/av-capture),
  Android 10 (API 29)+, needs `RECORD_AUDIO` and the same MediaProjection
  consent.
- Only players with usage `USAGE_MEDIA`, `USAGE_GAME` or `USAGE_UNKNOWN` can
  be captured; "all other usages CAN NOT be captured"
  ([reference](https://developer.android.com/reference/android/media/AudioPlaybackCaptureConfiguration)).
  Gwar's voice plays with `USAGE_VOICE_COMMUNICATION`, so it is **never
  captured**.
- Apps can opt out (`android:allowAudioPlaybackCapture="false"`,
  `setAllowedCapturePolicy`), and apps targeting Android 9 or lower are not
  capturable unless they opt in. Some games will share no sound; the UI says
  so when the captured audio stays silent.
- Gwar's other sounds (notifications, previews of shared audio) are kept out
  too: Gwar sets `ALLOW_CAPTURE_BY_SYSTEM` on its own players, and the
  capture configuration excludes Gwar's UID (`excludeUid`). Whether
  `excludeUid` combines with `addMatchingUsage` in one configuration is
  **(to verify)**; the capture policy alone is sufficient.

### iOS

**Video and audio, iOS 27 and later.**

- Apple's documentation now lists ScreenCaptureKit on iOS 27 and states that
  it "replaces ReplayKit for screen streaming and mirroring. A broadcast
  extension is no longer necessary"
  ([Apple](https://developer.apple.com/documentation/screencapturekit)). It
  recommends `SCContentSharingPicker`, a background execution mode for
  capturing in the background, and `NSScreenCaptureUsageDescription`.
- `capturesAudio` and `excludesCurrentProcessAudio` are listed for iOS 27 as
  well. If they behave as on macOS, iOS gets system audio without Gwar's
  voice.
- Everything about this path on iOS is new: which content can be captured
  outside the app, which background mode applies, and audio behaviour are
  **(to verify)** on iOS 27 devices before C9 is designed in detail.

**Before iOS 27: ReplayKit broadcast upload extension.**

- A Broadcast Upload Extension with an `RPBroadcastSampleHandler` subclass
  receives `.video`, `.audioApp` and `.audioMic` sample buffers in
  `processSampleBuffer(_:with:)`
  ([Apple](https://developer.apple.com/documentation/replaykit/rpbroadcastsamplehandler/processsamplebuffer(_:with:))).
  The user starts it from `RPSystemBroadcastPickerView` (iOS 12+)
  ([Apple](https://developer.apple.com/documentation/replaykit/rpsystembroadcastpickerview)).
- The extension is a **separate process** with a small memory limit, commonly
  reported as about 50 MB **(to verify)**. So it encodes in hardware
  (VideoToolbox) inside the extension and keeps no frame queue.
- It has no access to the main app's memory. It needs its own share
  transport, so its own connection to the server, or a hand-off of encoded
  frames to the main app through an App Group (shared container and a local
  socket) **(to verify which is allowed and practical)**. The signalling
  (share id, keys) goes through the App Group either way.
- Keys in the extension: in encrypted channels the extension needs the
  screen key for the current epoch. Passing MLS-exported keys to another
  process widens the key's exposure. The safer design keeps SFrame in the
  main app and hands only encoded plaintext frames across, which needs the
  main app to be running. **Design decision for C9.**
- `.audioApp` is the device's app audio. Whether it contains Gwar's own
  voice playback (a VoIP audio session) is **(to verify)**. If it does,
  shares from the extension have no sound.
- `RPScreenRecorder.startCapture` (iOS 11+) captures only Gwar's own app
  ([Apple](https://developer.apple.com/documentation/replaykit/rpscreenrecorder/startcapture(handler:completionhandler:))).
  It is useless for sharing other apps and is not planned.

The minimum iOS version for sharing is an
[owner decision](#owner-decisions-needed): iOS 27+ only (one path), or the
extension as well.

### Web (stays as is)

Unchanged from [screen-share.md](screen-share.md#browsers). For reference,
current MDN browser-compat-data (2026-10-15):

| | Chrome / Edge | Firefox | Safari |
| --- | --- | --- | --- |
| `getDisplayMedia` | 72+ / 79+ | 66+ | 13+, iOS 27+ |
| Audio capture | 74+: "On Windows and ChromeOS, the entire system audio can be captured when sharing an entire screen. On Linux and macOS, only the audio of a tab can be captured." | no | no |
| `systemAudio` option | 105+ (experimental) | no | no |
| `windowAudio` option | 141+ (`exclude` and `system` only) | no | no |
| `suppressLocalAudioPlayback` | 109+ | no | no |
| `restrictOwnAudio` | 141+ (experimental) | no | no |

Sources: [MDN getDisplayMedia](https://developer.mozilla.org/en-US/docs/Web/API/MediaDevices/getDisplayMedia),
[restrictOwnAudio](https://developer.mozilla.org/en-US/docs/Web/API/MediaTrackConstraints/restrictOwnAudio),
[browser-compat-data](https://github.com/mdn/browser-compat-data).

`restrictOwnAudio` removes the capturing tab's own audio. The web app plays
voice in that tab, so it works for the web client. It cannot help the desktop
app, whose voice plays natively outside the webview. That is why desktop
shared audio needs native capture.

## 6. Watching natively

A native viewer runs the watch transport in Rust (str0m frame mode, four
slots), opens SFrame, decodes and renders. The web viewer's behaviour is the
reference: minimal jitter buffer, keyframe recovery after key changes, and
the same pause and resume events.

### Decoding

| Platform | Hardware | Software fallback |
| --- | --- | --- |
| macOS, iOS | VideoToolbox (`VTDecompressionSession`); H.264 everywhere, VP9 on recent Apple silicon **(to verify per OS version)** | libvpx (VP9, VP8), OpenH264 decoder |
| Windows | Media Foundation decoder MFTs with D3D11 (DXVA) for H.264 and VP9 | libvpx, OpenH264 |
| Linux | VA-API, NVDEC | libvpx, OpenH264 |
| Android | MediaCodec, with `KEY_LOW_LATENCY` where supported (API 30+) | libvpx through the NDK |

Native viewers must decode VP9 and VP8 (every web sharer's codecs) and H.264.
A decoder error resets the decoder, marks the slot waiting for a keyframe and
sends `screen.keyframe`, as the web receiver does after a decoder reset
([Keyframe recovery](screen-share.md#keyframe-recovery-around-key-changes)).

### Rendering

| Platform | Renderer |
| --- | --- |
| macOS, iOS | `AVSampleBufferDisplayLayer` for decoded pixel buffers, or Metal (`CAMetalLayer`) |
| Windows | a D3D11 swap chain (flip model) in the UI's window |
| Linux | GTK4 dmabuf textures (`GdkDmabufTexture`) or a wgpu surface, depending on the UI toolkit |
| Android | `SurfaceView`; MediaCodec renders straight into its `Surface` |

Rules:

- **Present on the next refresh.** No extra buffering after decode. Frames
  that arrive late are dropped, not queued.
- **Video jitter buffer:** str0m frame mode delivers complete frames. Gwar
  adds the smallest reorder and completion buffer that keeps frames in decode
  order, with a target of zero, like `jitterBufferTarget = 0` on the web.
- **Scaling** happens in the renderer on the GPU, with the share's pixel
  aspect kept. Text shares are drawn at 1:1 pixels when the stage is large
  enough.
- **Multiple shares:** up to four slots decode at once. Only the stage share
  decodes at full rate; thumbnails may decode only keyframes or a reduced
  rate when the decoder is software **(design detail for D-items)**.

### Latency target

The same budget as [Latency budget](screen-share.md#latency-budget): p50
glass-to-glass at most 150 ms over the internet at 1080p30, and at most
100 ms within a region. Native sharing should save time in capture and
encode (no webview throttling, hardware encode at 2–8 ms). Native watching
should save time in the jitter buffer and render. Higher tiers keep the same
target; their larger keyframes only affect joins and recoveries.

### Shared audio playback

- Share audio is decoded (Opus, stereo, 48 kHz) in Rust and mixed into the
  same native output as voice, with a per-share gain and mute, remembered per
  sharer, as on the web ([viewer controls](screen-share.md#shared-audio-phase-2)).
- **The mix feeds the echo canceller's far-end reference.** Today the native
  engine's echo canceller (`sonora`) sees voice playback. Share audio must be
  in that reference too, or the viewer's microphone sends the share back
  into the call.
- Lip sync: share audio and video come with sender reports on one clock
  (the SFU's arrival wallclock); the viewer delays whichever is early, up to
  a bound **(to measure; F6)**.
- Ducking (lowering share audio while someone speaks) is optional and off by
  default.

## 7. Shared audio

Native shared audio follows [Shared audio (phase 2)](screen-share.md#shared-audio-phase-2)
exactly on the wire. Nothing new is needed on the server once screen-share
PR 9 lands.

- **Codec:** Opus, stereo, 48 kHz, `maxaveragebitrate=128000`,
  `useinbandfec=1`, no DTX and no RFC 6464 level gate (music must pass).
- **Transport:** its own `sendonly` audio m-line on the share's publish
  transport. On watch transports each slot is a video and audio pair, with
  one `msid` per slot.
- **SFU:** its own ingress bucket (192 kbps, 16 KB burst), forwarded before
  every video slot, no NACK.
- **Encryption:** SFrame with the label `"gwar screen audio v1"`, its own key
  and counter. The native engine reuses the voice SFrame code with that
  label.
- **Capture:** the platform `AudioSource` adapters of [§5](#5-capture-per-platform),
  resampled to 48 kHz stereo, encoded in 20 ms frames.

### Echo and the sharer's microphone

Two loops must not form:

1. **Gwar's voice into the share.** Prevented by capture exclusion
   ([§5](#5-capture-per-platform)). Without exclusion, no audio.
2. **Shared audio into the sharer's microphone.** On speakers, the sharer's
   microphone hears the game or video being shared. Viewers then hear it
   twice: through the share and through the sharer's voice.
   - The native engine has the captured share audio, which is close to what
     the speakers play. It feeds that signal into the sharer's echo
     canceller as an extra far-end reference, so the canceller removes it
     from the microphone.
   - The capture tap and the actual speaker output differ by the output
     path's latency. The canceller's delay estimation must cover it
     **(to measure; F6)**.
   - If F6 shows the canceller cannot remove it reliably, the fallback is
     the web design's: suggest headphones, and offer "Mute the shared audio
     for me".
3. Viewers already feed share audio into their canceller ([§6](#shared-audio-playback)).

### Changes to screen-share PR 10

[Screen-share PR 10](screen-share.md#phase-2-quality-and-reach) plans desktop
shared audio as native capture "fed into the webview's share as a track". The
capture adapters (C2, C4) are the same code either way. How their audio
reaches the share is open:

- through the webview, if WebKit and WebView2 can take a native audio source
  as a `MediaStreamTrack` **(to verify; WKWebView has no generic way that we
  know of)**;
- through the native publish transport (B4), as a second m-line beside a
  webview video m-line on a different transport, which the protocol does not
  allow today, or with native video as well.

If the webview route is not practical, desktop shared audio waits for native
publishing (B4 plus C1/C3), and PR 10 becomes items C2, C4 and B8 of this
plan.

## 8. Work breakdown

Each item is meant to be one pull request, merged on its own with CI green,
following the rules in [screen-share.md §7](screen-share.md#7-phases-and-pull-requests).
Sizes are working days for one person:

- **S:** 1–3 days;
- **M:** 4–8 days;
- **L:** 8–15 days.

"Now" means the item can start today on `main`, or on top of the open PR 3a
(#136) where it says so. ★ marks a good first contribution: small, testable
alone, and not on a security boundary.

Open an issue before starting an item, so two people do not build the same
thing. Items that touch encryption, keys or the SFU's limits get a review
round of their own, as voice 5c/5d had.

### Track A: SFU and protocol

**A1. H.264 payload parser ★** (S)
- Builds: `describe` for H.264 in `media/screen/ingress.rs`: single NAL,
  STAP-A and FU-A; frame start, keyframe start (SPS, PPS, IDR) and malformed
  payloads ([H.264 plan](#h264-plan)). No negotiation yet.
- Depends on: PR 3a (#136).
- Acceptance: unit vectors for each packet kind, including a STAP-A with SPS
  and PPS, an FU-A IDR start and continuation, a truncated STAP-A, an FU-A
  with S and E set, and reserved types. Parsing never reads past the payload
  (a fuzz test over random bytes).
- Now: yes, on top of PR 3a.

**A2. H.264 on the screen transports** (M)
- Builds: `enable_h264` with `packetization-mode=1` on publish and watch
  `Rtc`s; profile matching (Constrained Baseline, Constrained High);
  `level-asymmetry-allowed=1`; per-viewer payload type mapping; the
  "access unit must contain an IDR" check.
- Depends on: A1, PR 3a.
- Acceptance, in `crates/gwar-server/tests/screen.rs` (added by PR 3a) with
  str0m frame-mode clients:
  - an H.264 publisher reaches two viewers, frames byte-identical;
  - a late viewer starts at an IDR with its SPS and PPS;
  - an access unit with SPS but no IDR does not open a segment and causes one
    PLI;
  - publisher-side loss is repaired into place, as for VP9;
  - offers with `packetization-mode=0` only, or only unsupported profiles,
    are refused before negotiation.
- Now: after A1.

**A3. Codec capabilities and switching** (M)
- Builds: the rule in [Codec negotiation](#codec-negotiation-across-mixed-viewers):
  per-viewer decodable sets from watch offers, the sharer's encodable list
  from the publish offer, `screen.codec {share, codec}` to the sharer, slots
  that start only at a keyframe in a decodable codec, the 30 s hold before
  switching back, and `screen.stopped {reason: "codec"}`. Protocol types in
  `gwar-proto`, generated TypeScript, client validators.
- Depends on: A2. The owner decision on switching versus refusing.
- Acceptance: tests with an H.264 publisher, an H.264 viewer and a VP9-only
  viewer: the VP9 viewer joins, the publisher is asked to switch, both
  viewers decode after the switch keyframe, and the publisher is asked back
  30 s after the VP9 viewer leaves; a viewer flapping every 5 s causes at
  most one switch per 30 s; an empty intersection ends that watch only.
- Now: after A2.

**A4. Resolution tiers and scaled limits** (M)
- Builds: a tier per share (`1080p`, `1440p`, `2160p`) in `screen.start`; a
  server setting for the highest tier allowed (default `1080p`); the scaled
  frame bounds, buckets, transport meter and RTX cache of
  [SFU limits to raise](#sfu-limits-to-raise); admission and reservations by
  tier.
- Depends on: PR 3b; F3's measurements; the owner decision on defaults.
- Acceptance: a synthetic 2160p keyframe of 1.8 MiB passes in a 2160p share
  and is cut in a 1080p share; the RTX cache repairs the first packet of a
  2,000-packet keyframe; three overruns still end the share; a server with
  the default setting refuses a `2160p` start.
- Now: the code can start after PR 3b; the numbers need F3.

**A5. H.264 simulcast forwarding** (L)
- Builds: two RID layers on the publish m-line; per-viewer layer choice from
  PR 3b's allocation; switching at the target layer's keyframe, with a
  per-layer PLI; reservations by layer. Extends screen-share PR 13.
- Depends on: A2, PR 3b.
- Acceptance: a viewer whose estimate drops below the top layer moves to the
  low layer within one keyframe and is not paused; it moves back after 10 s
  of headroom; frames stay byte-identical across switches; Safari and Chrome
  H.264 simulcast senders work too.
- Now: no.

VP9 temporal layers (screen-share PR 8) and shared audio forwarding
(screen-share PR 9) are not repeated here. Native clients use them as they
are.

### Track B: shared Rust media core

**B1. `gwar-media` crate, traits and a test source ★** (S–M)
- Builds: the crate, the traits of [Adapter traits](#adapter-traits), frame
  types, and a synthetic `ScreenSource` that draws screen-like content (text
  scrolling, a moving block) with a frame-stamp barcode, as spike S3 did.
- Depends on: nothing.
- Acceptance: unit tests for the stamp encoder and reader (round trip
  through scaling by 0.5 and 2); docs on each trait; builds on all CI
  runners; no platform dependency in the crate's default features.
- Now: yes.

**B2. Video SFrame layouts in Rust** (M)
- Builds: VP9 whole-frame, VP8 clear-prefix and H.264 NAL-aware layouts
  ([§3](#encryption-nal-aware-sframe)) on top of the voice SFrame key state,
  per media kind, with the `"gwar screen v1"` label. Writes the H.264 byte
  rules into screen-share.md's [Frame layout](screen-share.md#frame-layout)
  and a shared vector file used by Rust and TypeScript.
- Depends on: the VP9/VP8 layouts are specified now; the H.264 rules are
  written in this item, together with E2.
- Acceptance: vectors at the shortest and widest KID/CTR encodings; an H.264
  frame whose ciphertext contains every start-code pattern round-trips;
  escaping is reversible for random inputs (property test); a str0m
  H.264 packetizer splits an escaped frame only at real NAL boundaries;
  mutation of any clear byte fails authentication.
- Now: yes for VP9/VP8; H.264 with E2.

**B3. Software codecs** (M)
- Builds: `soft` adapters: OpenH264 encode and decode (Cisco binary through
  `libloading` in releases, source build for tests), libvpx VP8/VP9 encode
  and decode, with the encoder settings of [§3](#hardware-encoders-per-platform).
  The download and verification of Cisco's binary.
- Depends on: B1. The owner decision on OpenH264.
- Acceptance: encode and decode the B1 source at 720p30 and 1080p30 on every
  CI runner; keyframe on request within one frame; bitrate within 15 % of
  the target over 10 s; the downloaded binary's hash is checked before
  loading; `THIRD_PARTY_NOTICES.md` and the cargo-about listing updated.
- Now: yes.

**B4. Native publish transport** (L)
- Builds: `screen.publish.offer` from Rust: a str0m frame-mode `Rtc` with one
  `sendonly` video m-line (H.264, VP9, VP8 and RTX); the encoder driven by
  the send-side estimate (`min(preset cap, 0.7 × estimate)`, as the web
  sharer does); keyframes on PLI; stats; the share lifecycle (start, stop,
  restart as a new transport).
- Depends on: B1, B3, A2 (for H.264).
- Acceptance, against a real `gwar-server` in-process:
  - a native publisher and a str0m viewer: frames byte-identical;
  - a Chromium and a WebKit viewer decode it (Playwright, with E1);
  - PLI produces a keyframe within one frame interval;
  - behind the `LATENCY_NET` relay's `rate=` bottleneck
    ([Loopback network impairments](latency.md#loopback-network-impairments)),
    the encoder's rate follows the estimate down within 2 s and back up
    within 10 s, without a pause;
  - start → stop → start creates a new transport and new SSRCs.
- Now: after B1 and B3; H.264 after A2.

**B5. Native watch transport** (L)
- Builds: `screen.watch.offer` from Rust: four `recvonly` slots, runs,
  `screen.slot` handling, SFrame open, gap tracking and `screen.keyframe`
  retries, the video jitter buffer of [§6](#rendering), and hand-off to a
  `VideoDecoder`.
- Depends on: B1, B3 (software decoders for tests).
- Acceptance: a Chromium sharer (Playwright) and a native publisher both
  play on a native viewer, with the frame stamp read back after decoding;
  a slot reassigned to another share starts at a keyframe; four slots at
  once; a run restart recovers all slots.
- Now: after B1.

**B6. Rate and quality controller ★** (S–M)
- Builds: pure logic mapping preset, tier, estimate and encoder or capture
  feedback to (resolution, fps, bitrate), with the degradation rules of
  [CPU and GPU budgets](#cpu-and-gpu-budgets).
- Depends on: B1 types only.
- Acceptance: table-driven tests: Text keeps resolution, Game keeps frame
  rate; no oscillation under a feedback signal that alternates every second;
  recovery only after 10 s of headroom; a codec switch to software VP9
  lowers the tier.
- Now: yes.

**B7. Native screen key endpoint** (M)
- Builds: a native `screen` endpoint under the call controller's per-kind
  endpoints (screen-share PR 7): `install`, `activate`, `hold`, `supersede`
  for screen keys; the counter that outlives transports; a lost encryptor
  counts as a restart with an own-leaf update. In the Tauri app, a
  `screen_set_keys` command beside `voice_set_keys`.
- Depends on: screen-share PR 7. B2.
- Acceptance: the PR 7 controller tests run against the native endpoint; an
  encrypted native share is watched by Chromium and WebKit; a removal stops
  the native sharer under the old key; a native publish transport restarted
  three times never repeats a counter (checked against the vectors).
- Now: no.

**B8. Shared audio in the native engine** (M)
- Builds: the audio m-line on publish and watch transports, Opus stereo
  encode and decode, `"gwar screen audio v1"` SFrame, mixing into native
  playout with per-share gain, and both echo references of
  [§7](#echo-and-the-sharers-microphone).
- Depends on: screen-share PR 9 (SFU part), B4, B5; an `AudioSource` (C2, C4,
  C6, C8 or C10) for real use, or a synthetic one for tests.
- Acceptance: a synthetic tone shared natively is detected at a Chromium
  viewer and a native viewer; mute and volume work; with a looped-back
  "speaker to microphone" path in the test harness, the shared tone is
  attenuated in the sharer's voice by at least 20 dB **(threshold to confirm
  in F6)**.
- Now: no.

**B9. UniFFI media facade** (M)
- Builds: the [UniFFI boundary](#the-uniffi-boundary): `MediaSession`,
  `ShareHandle`, `WatchHandle`, callback interfaces, and the
  `EncodedVideoSink`/`EncoderControl` pair for platform-owned encoders.
  Kotlin and Swift bindings generated in CI.
- Depends on: B4, B5.
- Acceptance: a Kotlin and a Swift test (JVM and macOS host, no device) drive
  a share from the synthetic source through a local server and receive
  stats; the facade exposes no key material.
- Now: no.

**B10. Native publishing in the Tauri app** (M)
- Builds: the first user of B4: `screen_*` commands in
  `apps/desktop/src-tauri`, a `native-screen.ts` engine beside the web
  `voice/screen.ts` (as `native-engine.ts` sits beside the web voice engine),
  and the command-surface table in architecture.md updated. The webview keeps
  the UI and, on macOS and Windows, keeps watching through WebRTC.
- Depends on: B4, A3, and C1 or C3.
- Acceptance: the `e2e:native-voice` run extended with a native share
  watched by Chromium and WebKit in the same call; the manual checklist
  (F5) on macOS and Windows release builds.
- Now: no.

### Track C: per-platform capture and encode

Each platform has a video item and an audio item. Video items include the
hardware encoder adapter. Each item adds its platform's manual checklist to
F5 and its numbers to F2.

| Item | What it builds | Depends on | Size | Now |
| --- | --- | --- | --- | --- |
| **C1. macOS video** | ScreenCaptureKit with `SCContentSharingPicker`, Gwar's windows excluded, `IOSurface` to VideoToolbox H.264 with low-latency rate control | B1, B4 | L | B1 first |
| **C2. macOS audio** | `capturesAudio` + `excludesCurrentProcessAudio`; Core Audio process tap as fallback; webview sounds handled | B1; B8 to send | M | B1 first |
| **C3. Windows video** | Windows.Graphics.Capture (Gwar picker through interop, system picker as option), frame-rate limiting, HDR to SDR, `WDA_EXCLUDEFROMCAPTURE`, Media Foundation hardware MFTs | B1, B4 | L | B1 first |
| **C4. Windows audio** | WASAPI process loopback excluding Gwar's process tree; version detection; no fallback to the system mix without the explicit setting | B1; B8 to send | M | B1 first |
| **C5. Linux video** | portal ScreenCast (`ashpd`), restore tokens, PipeWire dmabuf, VA-API or NVENC, software fallback | B1, B3, B4 | L | B1 first |
| **C6. Linux audio** | PipeWire per-application links excluding Gwar's nodes; PulseAudio-only and Flatpak detection | B1; B8 to send | M | B1 first |
| **C7. Android video** | MediaProjection consent per session, foreground service, virtual display into a MediaCodec `Surface`, encoded frames to Rust | B9; the Android client | M–L | no |
| **C8. Android audio** | `AudioPlaybackCapture` (media, game, unknown), Gwar's own players set to `ALLOW_CAPTURE_BY_SYSTEM` | B9; the Android client | S–M | no |
| **C9. iOS video** | ScreenCaptureKit on iOS 27 (after verification), or the broadcast extension with VideoToolbox in the extension | B9; an iOS client | L | no |
| **C10. iOS audio** | ScreenCaptureKit audio (27+) or `.audioApp` buffers, only if Gwar's voice is shown to be absent | C9 | M | no |

Acceptance for every capture item:

- **Video:** a 5-minute share of a moving test pattern at the preset's frame
  rate delivers at least 98 % of frames to a viewer, with Gwar minimized and
  with another app fullscreen; CPU and encode time within
  [the budgets](#cpu-and-gpu-budgets); the frame stamp decodes; stopping
  from the OS indicator (macOS menu bar, Android chip, Windows border or
  picker) ends the share in Gwar within 1 s.
- **Audio:** with Gwar playing a known voice signal and another app playing a
  different tone, the share contains the tone and **not** the voice signal
  (correlation below a threshold set in F6). This is the gate: an audio item
  that cannot pass it ships disabled.
- **Permissions:** first use, denial, revocation while sharing, and
  re-grant are each handled with a clear message (in Polish and English).

### Track D: native watch and render

| Item | What it builds | Depends on | Size | Now |
| --- | --- | --- | --- | --- |
| **D1. Apple decode and render** | VideoToolbox decode, `AVSampleBufferDisplayLayer` or Metal, libvpx fallback for VP9 where hardware lacks it | B5 | M | after B5 |
| **D2. Windows decode and render** | Media Foundation/D3D11 decode, flip-model swap chain | B5 | M | after B5 |
| **D3. Linux decode and render** | VA-API/NVDEC or software decode, dmabuf textures or wgpu, per the UI toolkit | B5; UI decision | M–L | after B5 |
| **D4. Android decode and render** | MediaCodec into a `SurfaceView` | B5, B9 | M | no |
| **D5. Watching in the Tauri Linux app** | a spike first: can a native GTK child view sit over the webview's stage area, follow its layout and stay in sync? If yes, D3's renderer in it | B5, D3 | S spike, then M | spike now |

Acceptance for D1–D4: glass-to-glass p50 within the
[latency budget](screen-share.md#latency-budget) on loopback, measured with
the frame stamp (F1); four slots decoding at once without dropping stage
frames; a decoder reset recovers through `screen.keyframe` within 1 s; no
frame is presented twice or out of order.

D5 matters because it is the only way to give Linux desktop users watching
before a native Linux UI exists.

### Track E: web-side changes

**E1. Web watch offers H.264 ★** (S)
- Builds: the web watch transport offers H.264 (Constrained Baseline,
  `packetization-mode=1`, and Constrained High where available) after VP9 and
  VP8. Web sharers are unchanged.
- Depends on: A2.
- Acceptance: Playwright: a str0m H.264 publisher is watched by Chromium and
  WebKit, with the frame stamp checked; Firefox by hand (F5); the existing
  VP9 tests still pass.
- Now: after A2.

**E2. H.264 SFrame in the web worker** (M)
- Builds: the NAL-aware layout's open (and seal, for screen-share PR 11's web
  H.264 sending) in the screen worker, against B2's shared vectors. This is
  the encryption part of screen-share PR 11.
- Depends on: B2 (vectors), screen-share PR 7.
- Acceptance: the shared vectors pass in WebCrypto; an encrypted native H.264
  share plays in Chromium and WebKit; worker cost per 2160p keyframe
  measured.
- Now: with B2.

**E3. Codec switch in the web client** (S)
- Builds: handling `screen.codec` as a sharer (switch codec on the same
  sender at a keyframe, through `setParameters` codec selection where
  supported **(to verify per browser)**, otherwise a new publish transport)
  and `screen.stopped {reason: "codec"}` as a viewer, with UI strings.
- Depends on: A3.
- Acceptance: Playwright: a Chromium sharer switches from VP9 to VP8 when a
  VP8-only test viewer joins, and back after it leaves.
- Now: after A3.

**E4. Higher tiers in the sharer UI ★** (S)
- Builds: tier choice in the preset menu, shown only up to the server's
  allowed tier and the sharer's capability (for web sharers: what the
  browser's encoder sustains, from `getStats`); strings in Polish and
  English.
- Depends on: A4.
- Acceptance: unit tests for the menu's rules; the tier is passed in
  `screen.start`.
- Now: after A4.

### Track F: measurements and QA

**F1. Native glass-to-glass measurement** (M)
- Builds: the frame-stamp method of spike S3 for native peers: stamp in the
  B1 source or as an overlay on real capture, read back after decode in the
  viewer; and a physical method for real displays (a camera filming the
  sharer's and the viewer's screens, or a photodiode) for release checks.
- Depends on: B1.
- Acceptance: a report like [latency.md's](latency.md#output): p50, p95, p99
  per run, with the machine, build and conditions recorded.
- Now: yes, with B1.

**F2. Encoder benchmark ★** (S)
- Builds: a `cargo run --example encode-bench` that runs each available
  encoder over the B1 source at each tier and reports encode time, CPU, GPU
  use where the OS reports it, and output bitrate.
- Depends on: B1, B3; hardware adapters as they land.
- Acceptance: results recorded in this document per machine.
- Now: yes for software encoders.

**F3. Keyframe and bitrate study ★** (S)
- Builds: measured keyframe sizes and bitrates for real screen content
  (code editor, document, web page, game footage) at 1080p, 1440p and 2160p,
  H.264 and VP9, using recorded clips through B3 and hardware encoders.
- Depends on: B3.
- Acceptance: a table in this document that sets A4's limits and §4's
  bitrates.
- Now: yes.

**F4. Voice gate with a native sharer** (M)
- Builds: PR 3b's paired voice gate rerun with the native publisher on the
  same machine as the native voice engine, so capture, encode and the screen
  transport compete with voice for CPU and uplink.
- Depends on: B4, PR 3b's gate tooling.
- Acceptance: the same gate as PR 3b (voice p95 and p99 within 2 ms of the
  paired baseline unconstrained; p99 within 20 ms constrained; underruns
  within baseline + 0.5 %).
- Now: no.

**F5. Platform checklists ★** (S, ongoing)
- Builds: one manual checklist per platform in this document: picker and
  permissions, minimized app, fullscreen game, multiple monitors with
  different scaling, HDR display, high refresh rate, sleep and lock,
  revocation, stopping from the OS indicator, audio exclusion with a live
  call.
- Depends on: nothing; it grows with each C item.
- Now: yes.

**F6. Shared-audio echo and lip sync** (S)
- Builds: measurements for [§7](#echo-and-the-sharers-microphone): exclusion
  of Gwar's voice per platform, the sharer's echo canceller with the share
  reference, and audio-video skew at a viewer with a stamped flash and click.
- Depends on: B8 and an audio C item.
- Acceptance: thresholds recorded and used by B8's and C's acceptance.
- Now: no.

**F7. Server cost at higher tiers** (S)
- Builds: SFU CPU per forwarded Mbps on the VM at 2160p packet rates, and
  egress with simulcast layers, extending PR 3b's measurement.
- Depends on: A4, A5.
- Now: no.

### Order

| Stage | Items | What it gives |
| --- | --- | --- |
| 0. Now | A1, B1, B3, B6, F2, F3, F5; D5 spike | parsers, traits, codecs and numbers; no user-visible change |
| 1. After PR 3a/3b | A2, E1, B2 (VP9/VP8), B4, B5, F1 | a native publisher with the test source, watched by browsers and by a native viewer |
| 2. First native sharing | A3, E3, C1, C3, B10, F4 | the desktop app on macOS and Windows shares natively with hardware H.264 (plaintext channels) |
| 3. After screen-share PR 7 | B7, B2 (H.264), E2 | native shares in encrypted channels |
| 4. After screen-share PR 9 | B8, C2, C4, F6 | desktop shares with sound, without Gwar's voice |
| 5. Linux | C5, C6, D3, D5 | the Linux desktop app shares (and watches, if D5 works) |
| 6. Native watching and mobile | D1, D2, B9, C7, C8, D4, C9, C10 | native viewers; Android and iOS sharing |
| 7. Higher tiers | A4, A5, E4, F7 | 1440p and 2160p with simulcast |

Stages overlap. Stage 7's A4 can move earlier for the Text preset alone,
which fits today's caps. Rough total: stages 0–2 about 10–14 working weeks
for one person, and the whole plan several times that. These are planning
figures, not dates.

## 9. Testing strategy

### Automated, on every CI run

- **Loopback through the real SFU.** Like `crates/gwar-server/tests/screen.rs`
  (PR 3a): an in-process server with str0m frame-mode clients as sharers and
  viewers. Native transports (B4, B5) are tested the same way, with the B1
  source and software codecs. Frames are compared byte for byte where no
  decoder is involved, and by frame stamp after decoding.
- **Impairments.** The `LATENCY_NET` relay with loss, reordering and the
  `rate=` bottleneck ([latency.md](latency.md#loopback-network-impairments)),
  as PR 3b uses it.
- **Interop with browsers.** Playwright with Chromium and WebKit, as
  `e2e/screen.mjs` (screen-share PR 4) and `e2e:native-voice` do: a native
  peer (through a stdio bridge like `native_bridge`, or a test binary) shares
  to browsers and watches browsers. Firefox is checked by hand until it runs
  in CI.
- **Vectors.** SFrame video layouts share one vector file between Rust and
  TypeScript (B2, E2).
- **Fuzzing.** Payload parsers (A1) and unescaping (B2) get fuzz targets;
  CI runs them briefly, longer runs are manual.

### CI constraints

- GitHub's macOS, Windows and Linux runners have no usable GPU encoder and
  no display to capture. Hardware encoders, real capture and audio exclusion
  are **not** in CI.
- Platform adapters build and run their unit tests in CI (`cargo check` per
  target, mocks for OS callbacks). Their real behaviour is tested by hand
  behind a cargo feature or an environment variable, like the desktop's
  `GWAR_CAPTURE_CHECK=1` check page ([Capture check](screen-share.md#capture-check-pr-1)).
- Software codecs run in CI. OpenH264 is built from source in CI, never in
  releases ([Software fallbacks](#software-fallbacks)).
- Android and iOS bindings are generated and compiled in CI; device tests are
  manual until a device farm exists.

### Manual, per platform (F5)

Each C and D item adds its rows. Results are recorded in this document with
the OS version, hardware and build, as screen-share PR 1's table does.

### Latency method

- **Frame stamp.** The source draws a 20-bit barcode with its capture time;
  the viewer reads it after decode and compares with the shared clock, as
  spike S3 did. Loopback only (one clock).
- **Physical.** For real displays: one camera filming both screens with a
  millisecond clock on the sharer's screen, or a photodiode on each screen.
  Used for release checks, not in CI.
- **Voice impact.** Paired runs and gates as in PR 3b and
  [voice-native-5d.md](voice-native-5d.md#latency), with ten pairs and
  alternating order.

## 10. Risks and open questions

### Risks

- **H.264 licensing.** H.264 is patent-encumbered. OS encoders and decoders
  (VideoToolbox, Media Foundation, MediaCodec) are licensed by the OS vendor
  **(to verify per platform's terms for distributors)**. OpenH264 is covered
  only through Cisco's binary. Linux hardware paths depend on how the
  distribution built Mesa. Mitigation: OS codecs first, Cisco's binary as
  the only OpenH264 in releases, VP9 everywhere else.
- **App store policies.** iOS broadcast extensions have tight memory limits
  **(to verify)**, and Apple reviews screen-capture apps. Google Play reviews
  foreground service types; `mediaProjection` must be justified in the Play
  Console declaration **(to verify current form)**. Android 14 consent per
  session cannot be avoided.
- **Games and anti-cheat.** Gwar never injects into games, so it should not
  trip anti-cheat. Some games and protected content (DRM video) render black
  in OS capture; exclusive fullscreen may not be captured on Windows
  **(to verify with Windows.Graphics.Capture)**. These are documented
  limits, not bugs.
- **HDR and high refresh rates.** HDR sources are tone-mapped to SDR before
  encoding (Windows float formats, macOS 15 dynamic range). HDR sharing is
  not planned. High refresh rate screens deliver more frames than the preset
  needs; adapters cap the rate.
- **Multiple monitors and DPI.** Captures are in physical pixels. Monitors
  with different scaling, and windows moving between them, change the
  capture size mid-share. Encoders must reconfigure (a keyframe) without
  ending the share. F5 covers it.
- **Privacy.**
  - Sharing a whole screen also shares notifications. Android 14's
    single-app sharing leaves them out; on desktops Gwar can only hide its
    own windows. The sharer is told before starting.
  - Gwar hides its own windows from captures by default (macOS filter,
    `WDA_EXCLUDEFROMCAPTURE`), so chats and other shares are not
    re-broadcast.
  - OS indicators (macOS menu bar, Windows border, Android chip) are kept.
    Turning off the Windows border needs the user's consent anyway.
- **Hardware encoder quirks.** Rate control overshoot on scene changes,
  session limits, driver bugs, and very different latency per vendor. F2's
  matrix and the software fallback contain it.
- **Bigger keyframes.** 2160p keyframes do not meet the 400 ms deadline for
  most viewers ([SFU limits to raise](#sfu-limits-to-raise)). Simulcast is
  the remedy; intra refresh (spreading a keyframe over several frames) is a
  later option where encoders support it, but the SFU's join logic needs a
  real IDR.
- **Voice impact on the sharer's machine.** Capture and encode compete with
  the voice engine for CPU, and the share competes for uplink. F4 gates it.
- **Two engines on the desktop during the transition.** The Tauri app will
  have native voice, native sharing, and webview watching at once. The call
  controller's per-kind endpoints (screen-share PR 7) must cover a native
  screen endpoint (B7), not only a web worker.
- **Unverified iOS 27 behaviour.** The iOS plan depends on how
  ScreenCaptureKit works on iOS. It is new.

### Changes to earlier plans

- **[Phase 3: native](screen-share.md#phase-3-native-and-peer-to-peer)** said
  native capture would come for macOS and Windows games "if needed", and for
  Linux only if the owner revisited the deferral. The owner's intent is now
  native media on every platform. This plan replaces that bullet. Owner
  decision 1 still holds for phase 1: the Tauri Linux app has no sharing
  until the native path (stage 5) exists.
- **"The desktop needs no new native command. Video keys never cross into
  Rust"** ([Two engines under one call controller](screen-share.md#two-engines-under-one-call-controller))
  holds for webview sharing. Native sharing needs screen keys in Rust (B7,
  `screen_set_keys`), with the same control contract as voice.
- **Screen-share PR 10** (desktop shared audio) may become C2, C4 and B8 of
  this plan ([Changes to screen-share PR 10](#changes-to-screen-share-pr-10)).
- **Screen-share PR 11** (H.264) gains SFU work it did not list: A1 and A2
  (H.264 keyframe detection and negotiation in the SFU). Its NAL-aware SFrame
  is E2 here, with B2's shared vectors.
- **Screen-share PR 13** (simulcast) is extended to H.264 and native senders
  (A5).
- **README's roadmap** item "Fully native clients later" now points here.

### Owner decisions needed

1. **Native desktop UI.** SwiftUI on macOS (shared with iOS), a Windows
   toolkit (WinUI through C# bindings, which UniFFI does not generate
   itself **(to verify third-party generators)**, or a Rust UI), and GTK4 or
   another Rust UI on Linux. Or one cross-platform Rust UI. Until decided,
   native media lands in the Tauri app.
2. **Higher tiers and caps.** Keep the 8 Mbps per-share cap and 300 Mbps
   budget ([owner decision 4](screen-share.md#owner-decisions)) and offer
   only what fits (Text up to 2160p, Smooth 1440p), or raise them as a
   server setting for admins who opt in. Default proposal: keep the defaults;
   admins may raise.
3. **Codec mismatch:** switch the sharer to a codec every viewer decodes
   (recommended), or refuse viewers that cannot decode H.264.
4. **OpenH264:** download Cisco's binary at first use for the software
   H.264 fallback, or ship no software H.264 encoder at all (VP9 when there
   is no hardware H.264).
5. **Minimum OS versions for native sharing:** macOS 14 (kept), Windows 10
   1903 for video and Windows 11 (or build 20348) for sound, Android 10 for
   sound, iOS 27 only or the broadcast extension as well.
6. **Windows without process loopback:** no shared sound (recommended), or a
   whole-system mix behind a warning with a separate voice output device.
7. **Linux distribution format.** Flatpak limits PipeWire access and may rule
   out per-application audio. Native packages do not.
8. **Hide Gwar's own windows from captures by default** (proposed: yes, with
   a setting to show them).
9. **iOS keys:** keep SFrame in the main app and hand encoded plaintext
   frames from the extension (proposed), or export screen keys into the
   extension process.

### Items to verify

Collected from the text above:

1. macOS: no Screen Recording prompt when capture goes through
   `SCContentSharingPicker` in a signed build.
2. macOS 15+: how often the system asks to re-confirm apps that capture
   without the picker.
3. macOS: whether a picker-chosen `SCContentFilter` can also exclude Gwar's
   windows.
4. macOS: whether `excludesCurrentProcessAudio` covers sounds played by
   WKWebView's helper processes.
5. Windows: whether an unpackaged app can declare
   `graphicsCaptureWithoutBorder`.
6. Windows: process loopback on Windows 10 builds before 20348 (reported to
   work from 19041).
7. Windows: WebView2 processes fall inside the excluded process tree.
8. Windows: exclusive-fullscreen games under Windows.Graphics.Capture.
9. Windows: whether Media Foundation hardware MFT latency is low enough, or a
   direct NVENC path is needed.
10. Linux: which distributions build Mesa without H.264 VA-API encoding.
11. Linux: current status of an audio portal; per-application PipeWire
    capture inside Flatpak.
12. Android: low-latency MediaCodec keys per vendor.
13. Android: `excludeUid` combined with `addMatchingUsage` in one
    configuration.
14. Android: Play Console declaration for `mediaProjection` foreground
    services.
15. iOS 27: ScreenCaptureKit scope (other apps, background mode, audio,
    `excludesCurrentProcessAudio`).
16. iOS: broadcast extension memory limit (about 50 MB reported); whether
    `.audioApp` contains Gwar's own VoIP playback; extension networking
    versus App Group hand-off.
17. Browsers: H.264 decode in Chromium builds without proprietary codecs;
    Firefox High profile; whether Chrome and Safari send SPS/PPS with every
    IDR; the levels browsers offer versus what they decode.
18. Browsers: switching codec on one sender with `setParameters`.
19. Apple decoders: VP9 hardware decode per OS version.
20. Hardware encoder session limits on consumer GPUs (for simulcast).
21. OpenH264: whether downloading Cisco's binary at first use (not at
    install) meets its terms.
22. OS codec licensing terms for distributors (VideoToolbox, Media
    Foundation, MediaCodec).
23. WebKit and WebView2: any way to take a native audio source as a
    `MediaStreamTrack` (screen-share PR 10's route).
24. UniFFI: foreign handles (surfaces, layers) and C# generators.
25. VP9 versus H.264 bitrate at equal quality for screen content (F3).
