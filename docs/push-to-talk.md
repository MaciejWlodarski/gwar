# Push-to-talk

Settings → Audio keeps the PTT key in the input-mode block, replacing the
voice-activity sensitivity controls. Choose a physical keyboard key (including
left/right modifiers), a middle mouse button, or either side button. Escape
cancels recording. The mouse icon also offers an explicit button picker, for
browsers/OS surfaces that use side buttons for navigation before delivering
them to the page. The release-delay slider applies to web and native audio:
0–1000 ms, initially 300 ms to retain the existing web tail.

On macOS and Windows PTT **observes input without consuming it**. A game or other
focused application still receives the original press and release. Mute/deafen
chords continue to use the existing OS shortcut registration.

## Privacy and permissions

The native callback matches **only the configured key or button**. Gwar never
stores, logs, or forwards any other input. It never decodes typed characters.
Only the bound input's pressed/released state crosses IPC. The callbacks update
atomics; voice control, recovery, and UI notification happen outside callbacks.
The listener exists only while PTT has a configured binding, and pauses during
binding capture.

macOS uses a **listen-only** CoreGraphics session event tap, with Input Monitoring
access checked using `CGPreflightListenEventAccess`. Gwar never requests a grant
on startup or in a retry loop. Once a global key is selected, a compact status row
says the microphone is closed and offers an Allow button. Only that explicit
click requests access once (registering Gwar with the OS); the button then opens
System Settings → Privacy & Security → Input Monitoring. Enable Gwar there (add
the development app manually if needed). The listener detects a grant without
another prompt; macOS may require restarting the app. Details are in a dialog,
with Polish and English UI text. Without access, even a focused-window key cannot
bypass the closed microphone. The call control also shows a closed microphone
with the plain-language reason; clicking it opens Audio settings. This is a setup
action with no pressed state or mute shortcut announcement, rather than a toggle.

macOS Caps Lock is explicitly unsupported for held PTT: its toggled state is not
a reliable physical hold. The saved binding stays visible with an explanation.
Windows Caps Lock and right/left modifiers are observed as physical scan codes.
Unsupported keyboard codes remain saved, rather than silently changing bindings.

## Failure recovery

The listener polls **only the bound input's** physical held state. A missed
release is a failure: physical release with an observed held state cancels
capture immediately, even with the maximum 1000 ms release delay. A poll can
close the gate, but cannot open it: opening requires a matched callback and a
release after listener installation/recovery.

The native audio pipeline checks a 250 ms lease itself. The observer renews it
about every 20 ms; callback stalls, dead workers, and suspension cannot leave the
microphone latched indefinitely. Wall-clock leases include sleep, and backwards
clock changes fail closed. An unavailable/disabled tap, missing callbacks, a
locked/off-console macOS session, or an unavailable Windows input desktop cancels
PTT without its release tail. Recovery installs a fresh listener and requires a
release before another press. A UI status poll restarts a stale supervisor.
Delayed webview PTT commands cannot reopen native global capture. Authorization
also belongs to one native voice owner/run. Switching or rejoining voice, or
replacing the webview owner, cancels the old press under the voice lock and
requires a physical release before a new press can authorize that run.

Observer health has its own 250 ms lease, separate from physical-state polling.
Only explicit OS health checks (every 100 ms) renew that authorization. A
latched callback state and a physically held key cannot renew a failed receiver.
A failed receiver must be reinstalled and released before it can open capture.

Windows uses passive Raw Input (`RIDEV_INPUTSINK`) and a message-only window.
It never sets `RIDEV_NOLEGACY`, so the focused app still receives normal input.
This replaces low-level hooks, which Windows can silently evict without a
queryable health status. The health check verifies that the receiver window and
its callback still exist and that `GetRegisteredRawInputDevices` still reports
our exact passive registration. Existing native subscriptions are saved and
restored when PTT stops; an old observer cannot unregister its replacement.
Only the bound key/button's state survives the callback; its temporary OS packet
is discarded, including positions, wheel data, and all unbound input.
See [Microsoft's monitoring recommendation](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc),
[Raw Input](https://learn.microsoft.com/en-us/windows/win32/inputdev/about-raw-input),
and [registration inspection](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getregisteredrawinputdevices).
The macOS implementation uses the [CoreGraphics event tap API](https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)).

## Linux and browsers

Linux retains its previous global keyboard shortcut where supported. Its compact
status explicitly says the key is reserved for Gwar: this is not passive input.
If the desktop rejects it (including Wayland desktops that do not allow it), PTT
works only in the focused Gwar window. Mouse buttons use that focused-window
fallback. The UI explains this without X11/Wayland terminology. Browsers also
use focused-window input and close PTT on blur. Passive global observation and
its physical-state recovery are currently implemented for macOS and Windows.

## Dependencies and validation

No dependency or lockfile was added or changed. The implementation calls OS
frameworks directly, so there is no new third-party license or advisory surface
to audit. The project remains MIT-licensed.

Automated coverage includes callback pass-through and bound-key isolation,
release-before-rearm, missing releases, sleep/clock discontinuities, audio lease
expiry, release-delay behavior, permission/recovery IPC, stale registrations,
and lossless settings migration from v6 to v7. The PTT recorder owns its listeners
inside the conditionally mounted PTT controls: changing input mode or hiding
those controls retires keyboard/mouse capture and the global capture flag.
Tests also cover owner/run replacement, independent health expiry, missing
release cancellation, Raw Input matching, setup-button accessibility, and
production MLS stale-commit retry (the browser fixture tolerates only `stale`). Browser screenshots exercise real
rendered settings with fixture permission states; they do not establish an OS
permission grant or physical keyboard/mouse behavior. Real macOS/Windows hardware
checks must still cover focused games/editors receiving the binding, mouse side
buttons, modifiers/AltGr, permission grant/revocation, sleep, lock/unlock, secure
input, and Windows receiver/registration loss.
