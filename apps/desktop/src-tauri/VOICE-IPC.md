# Native voice ownership and commands

The main window can call these app-owned Tauri commands. They are registered
in `src/lib.rs`; `capabilities/default.json` grants event listening. App-owned
commands do not need additional plugin permission entries. Every command below
checks the injected calling webview window and rejects any label other than
`main` before accessing voice state or opening devices/settings. The capability's
`windows` list alone does not restrict app-owned commands.

| Command | Ownership and effect |
| --- | --- |
| `voice_bootstrap {}` → owner | Called exactly once when `NativeVoiceEngine` is constructed. Under the voice lock, retires the preceding owner’s active and pending voice, microphone tests, answer and TeamSpeak sink; clears held PTT and returns a native-issued owner generation. |
| `voice_start {owner, run, serverHost, mode, binding, resumeAfter}` | Current owner only; claims a new run and cancels the previous one. `mode: "encrypted"` needs the full Gwar call binding (server key, session, channel, call, participation, group) and binds the run's SFrame state before negotiation; `resumeAfter` is the decimal restart floor or null. Direct TeamSpeak rejects encrypted mode; plaintext rejects a group or floor. |
| `voice_set_keys {owner, run, body}` → `{serial, epoch, result}` or `{serial, epoch, error}` | High risk: main window only, never a remote origin. Current owner and run only; any other run, a plaintext run or a run still negotiating answers `ignored`. Replies after the native RTC task applied the control (docs/e2e-build.md "Installation and activation"). |
| `voice_crypto_counters {owner, run}` → counters or null | Aggregate SFrame counters of the current encrypted run; never key material. |
| `voice_answer {owner, run, sdp?, error?}` | Current owner and matching pending run only. |
| `voice_stop {owner, run}` | Current owner only; retires this or older runs, including starts still pending in DNS or device opening. |
| `voice_play_cue {owner, cue, volume}` | Current owner and main window only. Accepts only the built-in score names and a clamped 0..1 volume. During voice, mixes local PCM into the selected output before the AEC render reference, independent of voice volume. Outside voice, opens output only (never the microphone). Deafen permits only mute/unmute/deafen/undeafen confirmations. |
| `voice_set_muted`, `voice_set_deafened`, `voice_set_ptt`, `voice_set_ptt_release_delay`, `voice_set_input_mode`, `voice_set_master_gain`, `voice_set_slot_gain`, `voice_set_devices` | Require `owner` alongside their existing arguments. Reject previous-view controls under the voice lock. |
| `mic_test_start {owner, test, input?, playback?}`, `mic_test_stop {owner, test?}` | Current owner only. A monotonically increasing test id fences cancelled starts and stale cleanup. Refuses capture while muted/deafened, in voice or in a hidden/unfocused window. Bootstrap and native focus loss/close also retire the preview. |
| `mic_permission {owner}` → `granted` / `prompt` / `denied` / `unknown` | Current owner only. Reads OS authorization without opening a device or requesting permission. Unknown platforms require an explicit preview gesture. |
| `audio_devices`, `open_mic_privacy_settings` | No ownership token: enumeration or a fixed OS settings URI. |

All voice events include `owner`; offers, state and `voice://crypto` events
also include `run`. `voice://crypto {owner, run, binding, state, reason}`
reports `securing` (`counter_exhausted`, `key_expired`) or `failed`
(`key_conflict`, `key_queue_full`); a failure also ends the run. No status is
an installation acknowledgement.

Device events: `voice://issue {owner, side, kind, message}` reports a device
problem (`no_device`, `permission_denied`, `failed`); `voice://device {owner,
side, kind, device}` reports that the devices sorted themselves out: `switched`
(audio moved to `device` without the user choosing it: a chosen device
disappeared or came back) or `recovered` (the problem reported last is over).
`voice_set_devices` moves running voice to the new choice at once, reopening
only the side that changed, without renegotiating.

The engine waits for bootstrap before sending commands, keeps that one token
for its lifetime and never reclaims ownership after being superseded. Runs
remain timestamp-seeded; owner generations are independent of run values.

Retirement cancels the startup future and releases audio guards immediately.
An OS device-open call already running on a blocking thread cannot be aborted;
it checks retirement before opening and closes any obsolete ports as soon as
that call returns. No obsolete ports reach negotiation or a retained handle.

`voice://test-level {owner, test, db}` belongs only to its matching local preview.
The Audio settings page reuses the call's level events while in voice. Outside
voice, the preview is scoped to a visible Audio page in voice-activation mode,
without self-mute or deafen. Permission must already be granted for automatic
capture; otherwise the user starts it explicitly. Preview audio is never sent.
The engine drains pending preview acquisition/cleanup before opening call audio.

`voice_set_gate_ready {owner, run, serial, ready}` closes native output before a
mode transition. Only the active run's latest serial may reopen it, after the
server acknowledges `voice.input_gate`. Mode revisions also discard old queued
frames and reset the local hangover. Native SDP advertises input-gate version 2;
automatic mode uses the legacy server criterion. Older servers keep automatic
activation; manual sensitivity is unavailable and unsupported PTT stays closed.

An explicit `playback: true` microphone test records up to three seconds of processed
48 kHz mono PCM in memory. It requests capture shutdown, then waits up to two seconds
for the device thread to acknowledge actual stream destruction before opening the
chosen output. A shutdown timeout prevents playback and reports a localized capture
shutdown failure. `voice://test-state {owner, test, state, error?, error_kind?}` reports `recording`,
`preparing`, `playing` and `idle`; only the matching owner/test may update the UI. Retirement,
including native window focus/visibility loss, immediately cancels both capture
and sample output. The default meter-only preview remains silent. No sample is
uploaded, written to disk or retained between tests. Output opening/playback
failures are reported to the matching UI test. `error_kind: "capture_shutdown"`
distinguishes unacknowledged input shutdown from an output-device failure.
The settings UI offers an explicit silent-preview retry after capture-shutdown
failure. Focus, device/settings changes and Settings remounts cannot retry it
automatically; an eligible user request is required.
Native state retains the previous input's shutdown acknowledgement independently
of its test/owner. `mic_test_start` refuses with `microphone_shutdown_pending` while
that input is opening or not yet destroyed, including after Stop or bootstrap.
Acquisition rechecks the gate under the ownership lock before calling device I/O.
Once destruction is acknowledged, a later explicit retry is permitted. The UI
maps this refusal to a short localized message and retains the silent retry action.

Passive global PTT owns native capture independently of the webview. While it is
configured, `voice_set_ptt` cannot open or close that gate: UI events only reflect
the listener state. The audio pipeline enforces a renewable 250 ms lease; stale
webview commands cannot renew it. `set_global_ptt` is main-window-only and replaces
the observer generation atomically. See [push-to-talk](../../../docs/push-to-talk.md).

Global PTT authorization is additionally bound to the native voice owner/run.
A lease renewal for a held press cannot transfer to a replacement run: the
voice lock cancels it, and the observer must recover and see a release first.
Observer-health proofs expire independently of bound-input physical polling;
missed release callbacks cancel immediately without the configured tail.

## Share sound

See [share-audio](../../../docs/share-audio.md#desktop-app-computer-sound-and-native-playback).
Every command except `share_capture_support` and `share_capture_allow` takes the voice `owner`
and refuses a superseded one or any window but `main`. Bootstrap stops a capture and drops
the playout left by the previous view. Neither direction holds a voice lock while media flows.

| Command | Arguments | Result |
| --- | --- | --- |
| `share_capture_support` | | `ok`, `permission` or `unsupported`. |
| `share_capture_allow` | | Asks macOS to list Gwar for Screen Recording. |
| `share_capture_start` | `owner, id` | Starts capturing. `id` must grow; errors are `permission`, `unsupported`, `failed` or `superseded`. |
| `share_capture_read` | `owner, id` | Interleaved 48 kHz stereo `f32`, raw. Waits up to 100 ms; empty means no sound yet, the error `closed` means over. |
| `share_capture_stop` | `owner, id` | Stops that capture; a stale id stops nothing. |
| `share_playback_open` | `owner, run, slot` | Claims `slot` for watch transport `run`. False while no voice engine runs. |
| `share_playback_close` | `owner, slot` | Stops that slot. |
| `share_playback_gain` | `owner, slot, gain` | 0 to 1, independent of voice volume. |
| `share_playback_frames` | raw body | `owner: u64, run: u64`, then per frame `slot: u8, rtp_time: u32, length: u16, opus`, little-endian. A malformed batch is refused whole. |

Sound goes only where it was asked for: a frame for a closed slot or an older run is
dropped, and the webview sends no frame ahead of the slot's volume.
