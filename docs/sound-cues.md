# Sound cues

Settings → Sounds (`Dźwięki` in Polish) has a master switch, an independent
volume slider, a switch for every event, and a preview beside each switch.
Everything defaults on, at 60% cue volume. Settings version 6 migrates earlier
preferences automatically; see [data migrations](data-migrations.md).

## Original score

`shared/sound-cues.json` describes 14 original one- to four-note cues as
`[frequency in Hz, start in ms, duration in ms]`. Each cue names a timbre, so the
groups are told apart by colour and rhythm, not by pitch alone:

| Group | Timbre | Character |
| --- | --- | --- |
| Joining and leaving voice | `voice` | Warm two-note pluck, mid range |
| Microphone and deafen | `control` | One or two short, dry, low ticks |
| Voice connection | `alert` | Reedy repeated pulse, then a step down or up |
| Messages | `message` | High, inharmonic bell |
| Screen sharing | `share` | Fast four-note run across two octaves |

A timbre sets the partials (`[multiple of the frequency, amplitude]`), how fast a
note decays, and a level that evens out loudness across the groups. Every note
keeps a smooth attack and release. There are no recorded samples, third-party
audio, dependencies, or audio downloads. All cues last 70–290 ms. At the default
volume the loudest sample is below 0.048 (about -26 dBFS); at full cue volume it
stays below 0.1.

Export listening copies with the Python standard-library script:

```sh
python3 scripts/export-sound-cues.py /tmp/gwar-sound-cues
```

It writes one 48 kHz mono PCM WAV per cue and a `README.txt` explaining them.
The exporter, browser player, and Rust player use the same score and envelope.
Players reject an unavailable timbre without substituting another sound. The
exporter validates every timbre before writing any files and reports an invalid score.

## When they play

- Joining is confirmed when voice actually connects. Connecting to a server
  without joining voice is silent. Leaving voice or disconnecting plays the
  falling leave cue.
- Peers entering or leaving your current voice channel produce activity cues.
  Welcome/resync snapshots, other channels, your own channel moves, and history
  do not produce peer cues.
- Mute/unmute/deafen/undeafen confirm actual changes, including keyboard and
  desktop tray actions. Unmuting while deafened produces one unmute cue.
- A previously connected voice call losing its media or signalling connection
  sounds once; its next successful connection sounds as a recovery.
  Recovery belongs to the original channel and ends on explicit Leave, channel
  removal, a move, or a terminal refusal. Refusals do not sound like transport
  failures. Automatic certificate renewal stays silent when voice returns.
  An accepted move still reports real transport loss before the target first
  connects; that first connection confirms joining the target. A delayed Leave
  completion cannot announce leaving, or reset cues for, a newer call.
  Password prompts, refused joins and redundant reconnect stops do not supersede
  an accepted Leave. Repeated Leave calls confirm the same participation once.
- Verified live mentions and DMs sound unless their conversation is already
  visible and focused. Own messages, history, edits, rejected messages, and
  duplicate deliveries do not sound. Sound switches are independent of system
  notification permission and the system-notification switches.
- A peer starting a share in your voice channel sounds. Your share ending
  through capture/encoder/transport failure or a server stop sounds; stopping
  it in Gwar, cancelling the picker, and deliberately leaving voice do not.
  A retained share recovering across a reconnect stays silent; a terminal recovery
  failure sounds once.

Activity shares one 750 ms budget, so a room filling up produces one cue.
Control feedback has its own 100 ms budget. Self join/leave, previews, and
connection alerts have separate 300 ms budgets. Dropped bursts are never
queued for later. Players allow at most two short overlapping cues so that a
connection loss does not hide a simultaneous unexpected share stop.

Deafen suppresses activity, messages, connection and screen cues, including
previews. Mute and deafen controls remain audible. Already playing activity
is silenced when deafen turns on. Voice volume and per-person volumes do not
change cue volume. There is no portable OS do-not-disturb query in the current
browser/Tauri notification APIs, so the app does not inspect private OS settings.

## Output and microphone isolation

The browser player sends local AudioBuffers through the voice engine's output
bus, after the voice-only volume/deafen gain. Both use the selected
`AudioContext.setSinkId` output or the existing media-element sink fallback;
browsers without device selection use the system default. Cues never connect
to the microphone, outgoing WebRTC track, or a screen publisher. Browser echo
cancellation remains enabled by default. Autoplay is unlocked by a gesture;
blocked background cues are discarded instead of replaying on focus return.

On desktop `voice_play_cue` requires the main webview and its current native
owner token. During a call Rust mixes the cue into the existing speaker queue
after voice gain/deafen and **before** `Processing::render`, so echo cancellation
gets the same reference that the selected voice output plays. It does not open
a second device or restart the call. A bounded pending list uses `try_lock` in
the audio pipeline. Outside calls, bounded output-only workers play on the
saved output device without opening the microphone. Starting voice, replacing the
webview owner, or changing the output retires earlier idle cues. Voice startup
waits for retired cue outputs to close. The local microphone preview opens capture
only, so idle cues do not compete with a silent preview output stream. Cues requested
during startup wait for the AEC-aware mix for at most 300 ms. No cue PCM enters the
capture/Opus/network path.

Echo cancellation reduces acoustic pickup from speakers; actual room acoustics
and hardware remain outside the deterministic tests. Headphones avoid acoustic
pickup, including when echo cancellation is disabled by the user.

## Validation and review artifacts

```sh
pnpm --filter web typecheck
pnpm --filter web lint
pnpm --filter web test
pnpm --filter web exec node e2e/sound-cues.mjs
CARGO_BUILD_JOBS=4 cargo test -p gwar-client --lib
```

The browser harness starts and stops its own loopback Vite server, blocks
external HTTP/WebSocket requests, exercises migration/persistence and live
preview without microphone access, and captures the real settings at desktop,
tablet and mobile widths. `E2E_SCREENSHOTS` selects the artifact directory.
The cues and screenshots still require the owner's listening and visual review.

The production build adds approximately 3.4 kB gzip of JavaScript over main
at `502473d`, across the same 27 JS chunks, with no shipped audio assets.
The owner review export contains 14 WAVs and six Polish PNGs (top/bottom at
1440×1000, 768×1024, and 390×844).

The visual inspection uses the existing Gwar graphite/cyan design and form
width. Desktop allowances cover only inherited rail typography/edge padding,
the full-height navigation and the existing 720 px form cap. Tablet and mobile
need no allowances. The new section's source lint has no findings.
