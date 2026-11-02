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

## Sound during a running share

A share that lost its sound, or never had it, takes it back without ending. The picture
keeps its transport, encoder and stream, so viewers see no restart, and they pick the
sound up from `screen.audio.state`, which they already poll every second. Viewer
transports always carry paired audio m-lines, so nobody rewatches.

There are two ways in, depending on whether the publish transport has an audio m-line:

| Share | Path | Protocol |
| --- | --- | --- |
| Started with sound, sound ended | `replaceTrack` on the existing sender | `screen.audio.resume` |
| Started without sound | Re-offer on the same transport, adding one Opus m-line | `screen.audio.add` |

`screen.audio.add` carries a new offer for the running share. The SFU refuses it unless the
sender is the sharer, the share is negotiated, not encrypted, and the offer keeps the
picture's SSRC and RTX SSRC. It validates the offer before applying it, so a refused offer
changes nothing: the share continues as picture only and the page rolls its offer back. An
accepted one lowers the picture's ingress cap by the same 128 kb/s that a share with sound
reserves from the start. The picture's bitrate target drops with it, which is the only
thing a viewer could notice, and only if the link was already full.

Renegotiation is used rather than an m-line every share carries, so a normal video-only
start is byte-for-byte what it was: the startup gate is unaffected, and video-only shares
pay nothing. The one addition to a start is a `screen.audio.add.capabilities` query after
the share is live, which nothing waits for.

### Consent and retries

- The sharer's ask stands from Start, or from turning sound on in a live share, until the
  capture comes up. While it stands, a capture that failed is retried after 5, 15 and 45 s, a
  missing permission is checked every 5 s, and both are tried when the window regains focus,
  which is when a permission was most likely just granted.
- A capture that came up and ended, by **Stop sharing sound**, by the OS, or by the browser's
  own stop bar, is never retried. It takes a new click. Retrying is for the capture that
  never came, not for the stream somebody ended.
- A retry is quiet. It does not show a toast, take focus or open anything. If it comes up,
  the bar changes; if not, the bar keeps offering **Turn on sound**.

### Controls

The bar shows **Turn on sound** / **Włącz dźwięk** where the status would be, as an icon below
the `sm` breakpoint. In the desktop app it acts at once, unless the permission is missing.
Otherwise it opens Details, which says what is wrong and offers the next step. The button
exists only when a click can do something: not on a server without the query, not on a
system without capture, not while a resume is already running.

On macOS `CGPreflightScreenCaptureAccess` can stay stale until the app relaunches, so a
missing permission no longer short-circuits the start: a real capture attempt is the only
sure answer. Whether capture works after a grant without a relaunch depends on the OS, so
Details says that reopening Gwar may be needed, and that it ends the share.

A browser has no way to add sound to an existing capture. **Choose a tab** opens a second
picker restricted to tabs. Its video is stopped at once and only the audio is kept, which
the e2e run confirms keeps Chromium's tab audio alive. The browser's own stop bar for that
capture ends the sound only. This needs `suppressLocalAudioPlayback`, so it is offered only
in Chromium browsers. Elsewhere the bar keeps its plain status.

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
Details has a **Stop sharing sound** action. Sound can come back during the same share;
see [Sound during a running share](#sound-during-a-running-share).

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

### Lip-sync

A viewer lines a share's picture and sound up once a second. Voice is never involved, and a share
without sound never comes through here. `apps/web/src/voice/share-sync.ts` holds the decisions.

Both sides are read where they come out: the picture from the video receiver's synchronization
source, and the sound from the audio receiver's in a browser or from the mixer in the desktop app,
which reports when a recent sample reaches the speaker (`share_playback_clock`). The SFU rewrites
RTP times per viewer, so only it can relate them: `screen.audio.sync` returns one audio and one
video RTP time of the same instant for the viewer's slot, taken from the sender reports' shared
wallclock, or `known: false` until it has them. A viewer asks at each look.

**Sync aims 40 ms late**, the middle of the range nobody notices, 45 ms ahead to 125 ms behind. Aimed
at zero, a link's ordinary noise keeps crossing the edge sound must not lead past. Every wait below
is measured from that aim.

**The picture waits.** Sound carries speech, and a picture a few tens of milliseconds later is the
smaller loss. `jitterBufferTarget` is a minimum, so a wait starts from the delay the receiver
already has. The first look moves the whole way, since nobody has seen the picture settle; later
ones move at most 40 ms at half the error, and nothing moves for less than 10 ms. A wait starts only
when sound is 45 ms past the aim, short of the range's edge: below that it would only add latency and
push sound towards the edge it must not lead past. A picture that
keeps agreeing gives 5 ms a second back, because a browser's own sync pulls each side up to the
other and a lag alone would never show that a wait is no longer needed.

**Sound waits only in the desktop app**, for a picture that is late with nothing left to give, for
a lead over 25 ms, in steps. Sound leaves the webview there, so its own sync has none to work with.
The mixer plays it 1 % slower, 10 ms a second and at most 400 ms in all, with the speed changing
over 100 ms; when the wait falls it plays 1 % faster until the queue is back. With no wait the
mixer reads exactly as before. While sound is not ahead it keeps giving a wait back. In a browser
the engine's own sync moves sound; a second controller on the same pair only drove both up.

A watch settles for ten looks before anything moves, per slot: its keyframes and buffers are still
landing, and the engine's own sync gets there by itself. A paused picture moves nothing, so
the waits are still there for the resume, and the next look corrects what the pause changed. Muting
and deafening release both waits, since nothing heard is left to line up with, and sound coming back
starts over like a new watch. A server without the query answers `bad_request` once and the viewer does not ask
again; a question lost to a timeout is asked again at the next look, and muting releases the waits
either way. A webview without `jitterBufferTarget` leaves the picture alone, and the desktop mixer can
still hold sound.

**A new watch starts together.** Its sound is silent until its first picture is on screen, so
nobody hears a share they cannot see yet, and nothing about the first frame changes. A picture that
is 1.5 s late stops holding the sound back, because sound matters more. Past that moment the
picture can still run 300–700 ms behind for a few seconds, while the sharer's encoder and uplink
ramp; that is the first viewer of a new share only, and sound does not wait for it, because the
picture catches up by itself and sound held for it would then lag.

Stopping sound and bringing it back is the case this does not cover. The publisher's RTP time stays
behind its clock by the length of the stop, so for 10–20 s the engine holds the picture 300–470 ms
late, with or without this, and sound leads by that much.

What this cannot fix is a picture that is not arriving: on a link jittery enough to collapse the
estimate it can stop altogether, and sound then leads whatever is done here. The numbers are in
[share-audio-validation.md](share-audio-validation.md#lip-sync).

### After a link stall

When a link stops for a while and comes back, everything it held arrives at once. A receiver that
takes that backlog plays seconds behind and unlearns it slowly: the SFU's queue is clear within
about 3 s, but 500–1600 ms stays in the viewer's jitter buffer for 7–10 s. The estimate falls with
it, to 3–4 Mb/s from tens, and at that rate a keyframe misses its deadline. Two such keyframes
paused the picture for more than 10 s, on a link that was back and proven seconds before.

So a viewer drops the backlog before its receiver sees it (`apps/web/src/voice/late-frames.ts`).
`RTCRtpScriptTransform` runs on each received frame and works out how much longer it took than the
quickest of the last few seconds, by the media's own clock. It only acts after arrivals stopped for
200 ms: frames that are late without that are a queue, which the receiver drains by itself. A picture frame over 500 ms late, or a
sound frame over 200 ms late, goes; in the desktop app sound leaves for the mixer, which drops late
frames itself. Keyframes are always kept. Nothing is shed for more than a second in a row: a link
that stays that slow is the link now, and it sets the new measure. Without script transforms a
viewer plays the backlog, as before.

The first frame of a backlog proves the link is back, so that is when the viewer sends
`screen.stalled`, and again at the first frame in time, because frames were dropped since any
keyframe in between. The SFU asks for a keyframe and puts the estimate back to what the watched
shares need with room for a keyframe, never past what the link carried in the last 15 s. It does
that at most once in 10 s on a viewer's word, and it undoes one more fall within the next second,
because the burst's own feedback can land after the word of it. The estimator then proves or
lowers it as usual.

The picture has just jumped, so one more move goes unseen: after the backlog lands, the first look
that finds the two sides over 45 ms apart moves the picture the whole way, once, while sound is
free to come down at its own pace. Sound is only ever dropped before its decoder, which conceals
the gap as it would a lost packet.

### Not included

- **The sharer's own echo.** Sound the sharer's speakers play can reach their microphone.
  Feeding the captured sound to the canceller ([native-media.md](native-media.md#echo-and-the-sharers-microphone))
  is not done yet; headphones avoid it.
- Mobile clients and the browser are unchanged.

Validation is in [share-audio-validation.md](share-audio-validation.md#pr-2-desktop-app).
