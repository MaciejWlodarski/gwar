# Voice and person audio UX

Audit rechecked against `origin/main` at `1a54b517` (2026-10-31).

| Item | Current-main finding | Change |
| --- | --- | --- |
| P2-8 | #204 provides a privacy-scoped processed level preview, but no audible sample. | A deliberate **Hear yourself** action records three seconds, closes capture, then plays the sample through the chosen output. Stop, navigation, blur, hidden state, mute/deafen, device/processing changes and voice startup cancel both recording and playback. Playback never resumes automatically. |
| P2-10 | Volume already persists by stable `Client.uid` and maps to browser gains and native slot gains. Local mute and a visible adjustment marker are missing. | Retain that identity and playback plumbing; add a separate local mute preference so unmute restores the volume. Show mute or a percentage in the tree. Always-visible person options open the same controls as the context menu, including on touch screens. |
| P2-11 | Devices and master volume are available only in Settings. | An always-visible device button in the voice panel and mobile chat controls opens the same device/volume component as Settings. Output selection is disabled with its existing explanation where unsupported. |
| P2-13 | Tree joins require double-click or a hidden desktop button. There is no voice-only Gwar channel type: Gwar channels combine chat and voice. | Single click/Enter joins TeamSpeak voice channels; spacers never join. Gwar chat stays safe to open without capture. Join voice is always visible in the tree and chat header. Existing password, permissions and encrypted-voice checks remain in `joinChannelInteractive`. |
| P2-18 | The reducer still appends peer join/leave, self join and reconnect lines despite #202's sound cues. | Remove these routine presence lines. Keep welcome content, messages, connection state, failures, moderation and other actionable notices. |

No whole audit item was already complete. The existing stable-uid volume persistence,
both playback engines' slot mapping, and #204's sensitivity layout were retained.
No production Gwar or TeamSpeak connection is needed for validation.

## Persistence

Settings version **7** adds `userMuted: Record<uid, true>`. `userVolumes` is unchanged;
a muted person's saved volume is retained and the engines receive zero. All earlier
settings migrate automatically without discarding devices, processing, shortcuts,
bookmarks, account preferences, sounds or session intent. Muted settings are local to
the device, just like existing volumes. A later concurrent settings migration must
renumber this version and its fixture before merging.

## Local visual and audio checks

Run Vite with `VITE_CONNECT_URL=http://127.0.0.1:1 pnpm --filter web dev --host 127.0.0.1`,
then `node apps/web/e2e/voice-people.mjs`. Set `E2E_SCREENSHOTS` to choose the output folder.
The fixture uses production components, synthetic participants/devices, a generated
microphone tone and a policy blocking external requests. It never opens the OS mic.
The check asserts that no live input track exists when actual browser sample playback
starts, cancellation is synchronous, mute preserves the stored volume, join controls
are visible, and dialogs fit at 1440, 768, 390 and 360 pixels in both themes.
Recording screenshots wait for a live level, then verify it follows a generated
tone being silenced and restored. The earlier empty bar was captured before the
first level update; the fixture input is not silent. Compact Polish and English
idle, recording and playback labels fit without clipping or wrapping. After sample
playback, the test verifies that recording callbacks and decoded buffers have been
released and the eligible automatic sensitivity meter has resumed.
Screenshots are approval evidence for the UI; they do not establish physical-device
or native audio quality. Native cancellation/ownership is covered by Rust and IPC
regressions without launching the installed app.

## Round-one regressions

Completed samples now retire their engine-owned run before reporting completion.
Both engines release their callbacks, and browser playback also clears its decoded
buffer. Natural completion and **Stop test** recheck all #204 privacy and permission
gates before restoring the silent sensitivity meter; audible playback never restarts
automatically. Native sample output uses #200's saved-device fallback decisions for
an unplugged or unopenable output.

The browser smoke suite waits for actual online/voice state after reconnect and
the opened, joined channel before testing history pagination. It retains peer
resynchronization, identity-pin and older-history assertions without relying on
removed presence notices. The remaining join-related sound-preview text in
`e2e/sound-cues.mjs` is an existing sound control, not a presence-notice wait.

## Round-two regressions

Native sample playback waits for an acknowledgement emitted after device-thread
stream destruction, including delayed destruction. The two-second deadline prevents
playback when shutdown cannot be confirmed, with a specific Polish/English error.
Guard drop and cancellation remain immediate stop requests. Playback rechecks
ownership after waiting. Browser samples wait for the current output route or its
completed fallback, then recheck cancellation; media-element playback also waits
for `setSinkId`. Regressions cover deferred context/element routing, replacement,
fallback failure and cancellation.
A short Preparing status covers shutdown, routing and decode waits after recording
has stopped. The browser fixture deliberately defers routing and verifies that
capture is closed and playback has not begun in this state.

The chat header applies the same top-level TeamSpeak spacer rule as the tree,
including channels promoted to the top level after their parent disappears. Spacers
have no Join voice action at desktop or mobile widths; Gwar names and nested
TeamSpeak channels remain joinable.

The reported TeamSpeak attachment/ban failure at `tests/support/mod.rs:112` is the
120-second bridge-ready setup wait, before the test connects clients or exercises
attachments and bans. No changes in this PR affect bridge/server startup or that
test's setup path. No production TeamSpeak connection was used to investigate it.

## Design scope and review

The app-screen direction is `true-black-flat`, keeping Gwar's existing palette and
Inter typography. The page question is whether the microphone works and whose audio
needs adjusting; the live level meter and per-person gain marker carry that answer.
The existing settings shell and #204's input-mode layout are preserved. Both themes
were reviewed at all four widths, including the separate compact mobile Join voice
row that avoids crowding the call controls. No design direction was redrawn.

The generic design inspector also sees the existing avatar circles as icon chips,
the inherited mobile chat `h2` as a missing page `h1`, and the deliberately empty
chat fixture as unused space. Its existing secondary chat metadata contrast warning
is outside these new controls. These inherited shell/fixture findings were retained;
new dialogs, device selectors and person controls were inspected directly. Fixture
participants (Maciej, Anna, Piotr) and headset labels are demonstration data only.
