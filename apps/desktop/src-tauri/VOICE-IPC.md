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
| `voice_start {owner, run, serverHost, mode, binding}` | Current owner only; claims a new run and cancels the previous one. Encrypted mode is rejected. |
| `voice_answer {owner, run, sdp?, error?}` | Current owner and matching pending run only. |
| `voice_stop {owner, run}` | Current owner only; retires this or older runs, including starts still pending in DNS or device opening. |
| `voice_set_muted`, `voice_set_deafened`, `voice_set_ptt`, `voice_set_input_mode`, `voice_set_master_gain`, `voice_set_slot_gain`, `voice_set_devices` | Require `owner` alongside their existing arguments. Reject previous-view controls under the voice lock. |
| `mic_test_start {owner, input?}`, `mic_test_stop {owner}` | Current owner only. Bootstrap also cancels a pending test and closes its capture. |
| `audio_devices`, `open_mic_privacy_settings` | No ownership token: enumeration or a fixed OS settings URI. |

All voice events include `owner`; offers and state events also include `run`.
The engine waits for bootstrap before sending commands, keeps that one token
for its lifetime and never reclaims ownership after being superseded. Runs
remain timestamp-seeded; owner generations are independent of run values.

Retirement cancels the startup future and releases audio guards immediately.
An OS device-open call already running on a blocking thread cannot be aborted;
it checks retirement before opening and closes any obsolete ports as soon as
that call returns. No obsolete ports reach negotiation or a retained handle.
