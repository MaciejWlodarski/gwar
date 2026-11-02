# Screen-share sound (P1-6)

A supported browser can share another tab's sound through the screen SFU (PR 1).
The desktop app can share the computer's sound, leaving Gwar's own sound out, and plays
share sound through the native mixer (PR 2, [below](#desktop-app-computer-sound-and-native-playback)).
The webview never plays share sound. Encrypted channels still cannot share
screens; neither change adds a plaintext exception to that rule.

## Capture and echo boundaries

The picker requests tab audio, excludes the capturing tab, system audio and
window audio, and requests `restrictOwnAudio`. Microphone noise suppression,
automatic gain and echo cancellation are disabled for this separate music/media
track; the call microphone settings are untouched. The local tab keeps playing;
the share preview is always muted. Returned audio from a screen, window or
unknown surface is stopped and removed, even if the browser ignored the
picker hints. Where the browser advertises `restrictOwnAudio`, the returned
track must confirm that it was applied or the sound is discarded. The desktop app's
picker explicitly requests no audio and discards any returned audio: its sound comes
from native capture instead. Neither browser nor native voice playback is intentionally
included in a system mix.

These are browser APIs, not a guarantee about every OS/browser. The
[Screen Capture draft](https://www.w3.org/TR/screen-capture/)
allows audio to be absent and picker hints to be ignored. Unsupported choices
produce a normal video share with a short explanation. Other applications
playing a call in the chosen tab, acoustic speaker-to-microphone leakage,
and platform implementations that misreport their capture source cannot be
excluded by Gwar's tab selection. Physical speaker/headphone AEC measurements
are separate from generated-audio transport tests.

## Publisher startup

With bundled audio, Chromium can construct the video encoder at its temporary
300 kb/s default before the transport activates the existing negotiated 4 Mb/s
start allocation. Feeding native frames during that interval applies bandwidth
resolution reductions that take seconds to recover. After making the offer,
the publisher detaches video input until the transport is connected and its
sender parameters have completed, then reattaches the same captured track.
The offered SSRCs, capture lifetime, start hint, congestion response and audio
priority stay intact. A stale or closed publisher cannot reattach its source.
Video-only publishers keep their existing negotiation/input path.

Share Opus is fixed at a 64 kb/s ceiling and priced at 128 kb/s including wire
overhead in every publisher video/uplink budget. The publisher negotiates
transport-wide congestion feedback for video only. Including sparse share-audio
packets in that feedback depressed measured video capacity in the mixed
transport; the fixed audio allowance already accounts for their load. Audio
retains its RTCP clock reports, CNAME, RTP identity and other extensions. Voice
has its own unchanged feedback and transport; viewer audio still enters the
SFU's aggregate release and wire pacing.

## Compatibility

`screen.audio.capabilities {}` returns `{ "screen_audio": true }`. It is a
separate query, following the existing quality/codec negotiation pattern.
It changes no challenge, Welcome, existing event, share record, enum or
protocol major. Older clients never receive an unknown field.

A new publisher queries before publishing. If the query is rejected, times
out or returns no support, it stops/removes captured sound and publishes the
legacy video-only offer. A new viewer queries before building its offer and
uses the legacy four video slots when unsupported. A new SFU accepts both
layouts: four video m-lines, or four interleaved video/Opus pairs. Every watch
m-line remains receive-only. A publisher may add exactly one send-only Opus
m-line after its existing video/RTX m-line. Legacy video viewers can watch
new audio publishers without receiving audio. Legacy publishers work with new
viewers; their audio slots stay silent and show no audio controls or autoplay prompt.
`screen.audio.state { share, stopped?: false }` explicitly reports current audio
availability. Viewers query it for their current grant and refresh once per second;
an unknown query or failed refresh means no audio playback. The publisher alone
may send `stopped: true` when its retained audio track ends. No unsolicited event
or field is added for older clients. Guest watch pages retain their existing
video-only behavior.

No new persistent server data is introduced. Settings v11 adds the desktop app's
`shareComputerSound` consent, off for everyone until asked. Settings v10 adds `shareAudio`,
keyed by share id, with volume 0–100% and independent mute. It preserves
all v1–v9 fields: cues and sensitivity, desktop/notification choices in v7,
PTT bindings and release delay in v8, and person volume/mute in v9. Fixtures cover
every version and upgrade/reload. Main's #206/#208/#207 migrations retain their
numbers; this branch follows them at v10. Earlier v7/v8 share-audio preview maps
also survive. The Polish settings labels remain Głos and Dźwięki aplikacji.

## Voice priority and bounded forwarding

Voice keeps its separate transport and task. The screen task never decodes
share audio and never sends it through voice processing or sound cues.

- Browser Opus is capped at 64 kb/s with `priority` and `networkPriority`
  `very-low`; video remains `low`, voice keeps its existing priority. These
  browser priority hints are not an enforceable network reservation.
- The publisher subtracts a conservative 128 kb/s audio wire allowance from
  the existing video cap and uplink screen budget. Audio and video combined
  therefore have no larger planned budget than a video-only share. Existing
  missing-estimate and very-low-estimate limitations still apply.
- Audio ingress has separate 128 kb/s byte and 100 packets/s buckets, with
  2,400-byte/four-packet bursts and at most 1,200 payload bytes per packet.
  Its SSRC and negotiated Opus payload type are checked. Video ingress loses
  the same allowance; the existing whole-transport ingress meter still applies.
- Each granted slot of a share that actually negotiated audio reserves 128 kb/s *inside* the viewer's existing
  `0.8 × estimate` screen allocation. Video uses what remains. Audio waits in a queue of at most four packets with a 100 ms deadline,
  released before video through one aggregate screen pacer. Rate credit stays at
  the existing 80% media plus 10% repair/probe allocation and retains one packet
  of remainder instead of clipping it every service interval. A shared peak
  guard permits at most 10 ms at the estimated link rate, with a one-datagram
  floor including UDP/IP. Regular coarse service can use a window up to 30 ms;
  longer gaps reset the window rather than accumulating a catch-up burst. Audio
  and video configure this window together before either releases. A second
  wire gate checks queued audio, video, repairs and probes before str0m removes
  packets or assigns TWCC send records. Exhausted credit defers a packet; it
  does not turn application pacing into reported network loss. During a bounded
  capacity probe, that same gate follows its dispatched rate and existing extras
  reserve, retaining the occupied wire clock across plan changes. The
  separate audio allowance cannot buy an additional wire burst;
  repairs/probes retain their own cap too. Voice headroom is unchanged. Expired
  sound drops and never raises the probe target.
- A video-only watch has no audio reservation and no optional wire gate
  installed, including when its transport negotiated unused audio receivers.
  Starting audio installs the gate; ending its final audio reservation removes
  it and its pending retry deadline.
- Each negotiated audio/video pair shares one SDP stream ID and RTCP CNAME,
  with separate track IDs. Audio uses stable source sequence/timestamp offsets.
  Publisher sender reports map both source clocks through one common NTP anchor;
  a decrease in path delay moves that common anchor back so mapped wallclocks
  never lie ahead of packet receipt and str0m can keep producing valid reports.
  source wallclock is kept separately from queue arrival/deadline and release.
  Before sender reports arrive, the common SFU arrival clock is the fallback. New sources restart the mapping without restarting the
  viewer transport. No Opus RTX/NACK queue is added. Sender reports keep the
  audio/video wallclock association; perceptual lip-sync is still an acceptance
  measurement, not established just by forwarding RTP.
- The share lease, grant and recovery fence apply to audio. Revocation clears
  pending audio output; the socket checks the grant again. Budget-paused slots
  send no audio. A sharer is excluded from its own audio forwarding and playback.

## Playback and reconnect

Browser viewers have one audio element per granted remote share with confirmed
audio availability, independent of stage,
thumbnail, fullscreen and picture-in-picture video elements. Confirmed audio
availability survives bandwidth pause, waiting-for-picture and resume transitions
of the same grant; bandwidth pauses still permit audio. Volume and mute survive
stage changes and republishing under a new share id (the controller copies preferences only
when it has identified the confirmed replacement). Shares from two devices of
the same person retain independent controls. Deafen
mutes it; selecting an output device applies `setSinkId` where supported.
Browsers without that API use their normal output. Autoplay refusal offers
an explicit Play share sound button only for an audio-enabled share. Desktop
viewers have no player or sound prompt: their sound plays natively, below.

Reassigning a previously used audio slot replaces the watch peer connection
before granting playback. New DTLS keys, SSRCs and receiver buffers fence delayed
packets of the previous share. Other watches are restored on the new transport
and may briefly show Connecting. Legacy video-only transport reuse is unchanged.

The sharer's compact sharing row keeps the watcher count beside sound status
(or shows Sharing with sound before anyone watches). At phone widths the sound
state uses an accessible icon so both buttons and the count stay in one row.
Its details dialog contains quality settings, preview and
statistics. An active TeamSpeak link keeps its guest count and turn-off action
in the desktop row; phones use an accessible link icon with the full summary and
turn-off action in Details. The watcher count includes those guests. English and
Polish strings are included. The captured audio track
belongs to the same retained MediaStream as video: reconnect reuses it without
another picker, including encoder recovery. An ended track or a replacement
server without audio support leaves the video running and reports no sound.
Stopping or leaving stops all captured tracks.

Validation results, commands and the calibrated latency-gate limitation are in
[share-audio-validation.md](share-audio-validation.md).

## Desktop app: computer sound and native playback

The webview stays the transport. It keeps its peer connections and the same Opus
m-lines as a browser, so this adds nothing to the protocol, the SFU or its budgets:
share audio is still 64 kb/s, `very-low` priority, 128 kb/s in every budget, and
retained across reconnect. Native code owns capture, decoding, mixing and echo
cancellation. Details of the commands are in
[VOICE-IPC.md](../apps/desktop/src-tauri/VOICE-IPC.md#share-sound).

### Sharing

The quality menu asks once, with **Share computer sound** / **Udostępniaj dźwięk
komputera**. It is off until the sharer turns it on, and the answer is remembered
(settings v11). Starting the share is the consent: capture never begins by itself.
Details has a **Stop sharing sound** action; turning it back on needs a new share,
since the sender's m-line exists only from the start.

```
OS capture → CaptureQueue (200 ms, oldest goes) → share_capture_read (awaited, raw f32)
           → AudioWorklet (starts at 30 ms, holds 400 ms) → MediaStreamAudioDestination → Opus sender
```

A busy machine hands sound over in lumps, so the worklet only drops what stayed queued for
half a second: sound that drains before the next lump is on time, and a real stall catches up
within a second. The worklet's track goes only to the sender, never to `context.destination`. The read
loop waits on the shell's own promise, so a hidden window's timers do not slow it. The
`AudioContext` is made in the Start click's own task. A capture that hangs is abandoned
after 4 s: sound never holds the picture back. Whatever goes wrong, the picture
continues and Details says why, in plain words.

A capture cannot outlive the share that asked for it. A stop at any step of the start
cancels it, however late the OS answers: start, stop and the final install share one lock,
and the install also checks that the webview still owns the app. Nothing here relies on
JavaScript cleaning up: a page load, a new view or a failed WebView2 process ends the capture.

Five seconds without a read pause it instead: the OS stops listening, viewers hear silence, and
the shell keeps only the share's id and owner. A stalled renderer, a suspended webview and a
sleeping machine all look like this, and none of them took the sharer's consent back. When reads
return, the webview asks to resume that id; the shell restarts the OS capture for the same id of
the same owner, and for nothing else. Stopping the share, a newer start or a lost webview clears
what was kept, so nothing can resume after them. The webview cannot update its own page while it
is stalled, so the bar says "Restoring sound" from when it runs again until the capture is back,
and Details keeps "Stop sharing sound" throughout. A refused resume is tried three times, a second
apart, for a machine that has just woken; after that, or if the OS withdrew permission, the
share goes on without sound and Details says why.

The clocks differ across sleep: macOS's stops, so a nap may not reach the five seconds, while
Windows's runs, so waking there pauses first and resumes on the next read. A capture the OS itself
ended stays ended: on macOS the user can stop it from the menu bar, and that must not come back.
Every restart of a share carries the same consent, so a stop that arrives late, from a stream
already replaced, ends the one running now. Its sound stops being passed on at once, and the OS
stream within a second. A newer share has its own consent. Only a stream that came up can end
it: one that failed to start is a refused resume, which leaves the consent and can be tried again.

- **macOS 14 or newer:** ScreenCaptureKit with `capturesAudio` and
  `excludesCurrentProcessAudio`. It needs Gwar turned on under Screen & System Audio
  Recording, and a relaunch after granting it. The dialog offers to open System Settings; asking also adds Gwar
  to the list there. The framework is weakly linked, so macOS 11 to 13 still run Gwar
  and simply cannot share sound.
- **WebKit is not covered by that exclusion.** Measured here on macOS 27: a tone played
  by the app's `AVAudioEngine` was excluded (−240 dB), one played by its `WKWebView`
  arrived unchanged (−50.6 dB against −50 dB played). Its helpers are not in
  `SCShareableContent.applications` either, so an application filter cannot help. Voice and cues
  are native, so only chat attachments play in the webview: on macOS
  `silenceWebviewMedia` keeps every `<audio>` and `<video>` there muted from before the
  capture starts until the shell has stopped it. A chat video played during a sound share
  is silent.
- **Windows 11 or newer (build 20348+):** WASAPI process loopback excluding Gwar's
  process tree, which contains its WebView2 helpers. Earlier builds fail closed; a
  whole-device loopback, which would contain the call, is never used.
- Any other system, or a failed check: the picture alone, and the UI says so.

### Watching

A desktop viewer's audio receivers each get an `RTCRtpScriptTransform` before any
description is made. The worker posts every encoded frame out and writes none back, so
the webview has nothing to decode. `shareSoundReceivable` asks the real worker first;
without transform events the app stays video only, with no sound claim.

```
worker → share_playback_frames (raw batch, one in flight, 400 frames at most)
       → SharePlayout thread: ordered jitter queue (40–160 ms), Opus decode, mix of up to 4 shares
       → ShareBus (40 ms ring) → voice pipeline → speakers + echo canceller reference
```

- **Voice comes first.** The playout thread has ordinary priority. The voice pipeline
  takes the ring with `try_lock` and never waits for it. Share sound only uses the room
  voice leaves, so a loud share can clip but the voice under it cannot.
- **Echo:** the canceller's reference is the final left and right channels, voice and
  share together, clipping included, so a viewer on speakers does not send the share back.
  Both go to the device as pairs through one ring, so they cannot slip apart. Voice alone
  keeps one channel, to the bit what it was before share sound existed. The first non-silent
  share sound that actually plays pins a stereo canceller for the rest of that voice session,
  even after the share ends, so hopping between shares switches only once. A sharer, a deafened
  viewer and a viewer who muted the share stay on one channel. A reconnect replaces the
  pipeline, so it starts on one channel and pins again at the next share sound.

  A worker starts with each voice session, before the pipeline is promoted, and trains a stereo
  canceller beside the mono one on the same microphone and speakers, off the voice thread. The
  voice thread only copies frames into a fixed ring and never waits. Voice alone still comes out
  of the mono canceller, to the bit. Past half a second of silent speakers the worker hears
  nothing, so a quiet session costs a sleeping thread. The pin, taking the canceller and the
  take-over touch neither the heap nor a lock.

  What happens at the first share sound depends on whether that canceller found an echo, which
  it counts as having taken the microphone down by 12 dB at some point:

  - **Echo found, speakers.** It takes over 5 frames of sound after the pin; a raised-cosine frame
    hands over. It needs 3 s of sound behind it, or it hears the rest beside the mono one first.
    Wide sound does not leak and voice echo does not rise.
  - **No echo found.** Headphones, very quiet speakers, or nobody has yet talked alone at the far
    side. It is left behind and a new stereo canceller hears 3 s of sound beside the mono one, with
    voice still from the mono one, as it would without any of this. Headphones have no leak to stop,
    and a trained canceller holds the near voice down harder at new sound than a new one does.
    Wide sound does leak here, on quiet speakers and on speakers nobody has heard alone yet.

  What it learned is only good for that room and that stretch of the session, so a new microphone
  or speakers start it over, and so does a frame it lost for being a whole ring behind, or the
  canceller being turned off. Whatever it has found before then counts for nothing. It takes over
  only having heard every frame up to the one it takes over at, and it gets a quarter of a second
  to catch up: a later one is refused and the second way follows, voice on the mono canceller
  throughout. Once in charge it still knows it learned one room: at a new device, however late the
  news comes, the worker trains another for 3 s of sound, and that one replaces it in a single
  frame.

  A reconnect starts over, so with a share already playing it takes the second way
  (see [validation](share-audio-validation.md#warm-stereo-canceller)). Vendored Sonora adds the one option
  this needs ([`vendor/sonora/GWAR-PATCH.md`](../vendor/sonora/GWAR-PATCH.md)). Selecting an
  output device needs nothing new.
- **Controls:** the same per-share volume and mute as a browser (`shareAudio`), independent
  of voice volume. They appear only when the server confirms sound and the mixer has
  taken it. Deafen silences it without falling behind. There is no autoplay prompt.
- A newer watch transport retires everything older, by run. Open, close and volume each
  name their run and are refused below the slot's newest, which a slot remembers after it
  closes. They do not share the frame queue, so no backlog can lose one, and `open` only
  reports success once it is in force. Playback is not tied to one voice run: a voice
  reconnect replaces the pipeline reading the ring, not the streams.
- A lost frame is concealed up to 100 ms. Sound that arrives after the queue ran dry still
  plays, which leaves the queue the cushion the next lump needs. A queue that stayed over
  its limit (the delay plus 80 ms) for half a second is a stall's backlog: the excess is
  dropped rather than played late. So after a stall, sound can trail by up to 640 ms for half
  a second.

### Not included

- **The sharer's own echo.** Sound the sharer's speakers play can reach their microphone.
  Feeding the captured sound to the canceller ([native-media.md](native-media.md#echo-and-the-sharers-microphone))
  is not done yet; headphones avoid it.
- **Lip-sync is not actively corrected.** Sound runs behind the picture by the jitter queue and
  rings, by estimate at most 100–120 ms. It has not been measured.
- Mobile clients and the browser are unchanged.

Validation is in [share-audio-validation.md](share-audio-validation.md#pr-2-desktop-app).
