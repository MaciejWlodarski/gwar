# Browser screen-share sound (P1-6, PR 1)

This is the browser/transport half of P1-6. A supported browser can share
another tab's sound through the screen SFU. Desktop capture and viewing remain video-only. The webview never receives or
plays share sound: native playback must first enter the native mixer and AEC reference. Encrypted channels still cannot share screens; this change
adds no plaintext exception to that rule.

## Capture and echo boundaries

The picker requests tab audio, excludes the capturing tab, system audio and
window audio, and requests `restrictOwnAudio`. Microphone noise suppression,
automatic gain and echo cancellation are disabled for this separate music/media
track; the call microphone settings are untouched. The local tab keeps playing;
the share preview is always muted. Returned audio from a screen, window or
unknown surface is stopped and removed, even if the browser ignored the
picker hints. Where the browser advertises `restrictOwnAudio`, the returned
track must confirm that it was applied or the sound is discarded. Desktop capture explicitly requests no audio and also discards
any returned audio. Neither browser nor native voice playback is intentionally
included in a system mix in this PR.

These are browser APIs, not a guarantee about every OS/browser. The
[Screen Capture draft](https://www.w3.org/TR/screen-capture/)
allows audio to be absent and picker hints to be ignored. Unsupported choices
produce a normal video share with a short explanation. Other applications
playing a call in the chosen tab, acoustic speaker-to-microphone leakage,
and platform implementations that misreport their capture source cannot be
excluded by Gwar's tab selection. Physical speaker/headphone AEC measurements
are separate from generated-audio transport tests.

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

No new persistent server data is introduced. Settings v10 adds `shareAudio`,
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
viewers negotiate video only and have no player, volume control, sound prompt
or sound claim. Native playback through the voice mixer/AEC reference belongs
to PR 2.

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

## PR 2: native process-excluding capture

This is a separate implementation, not a small extension to the webview picker.
Follow [native-media.md](native-media.md#7-shared-audio):

- macOS: ScreenCaptureKit audio with `capturesAudio` and
  `excludesCurrentProcessAudio`; verify WebKit helper-process exclusion as well
  as native voice/cue exclusion, and fail closed where exclusion is unavailable.
- Windows: WASAPI process loopback excluding Gwar's process tree, including
  WebView2 helpers. Unsupported Windows versions remain video-only; do not
  substitute whole-device loopback that includes the call.
- Own the native capture lifecycle, consent, timestamps and a bounded bridge
  into the share's Opus sender; preserve picture/audio together on reconnect.
- Native playback must feed share sound into the voice AEC reference alongside
  voice, with per-share controls and the selected output device.
- Run native generated-tone, process-exclusion, physical AEC, lip-sync and paired
  voice-latency acceptance on both platforms. Neither production services nor
  the installed Gwar application are needed for this work.
