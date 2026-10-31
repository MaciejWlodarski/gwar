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
with the plain-language reason; clicking it opens Audio settings.

macOS Caps Lock is explicitly unsupported for held PTT: its toggled state is not
a reliable physical hold. The saved binding stays visible with an explanation.
Windows Caps Lock and right/left modifiers are observed as physical scan codes.
Unsupported keyboard codes remain saved, rather than silently changing bindings.

## Failure recovery

The listener polls **only the bound input's** physical held state. A missed
release closes capture even if the release callback never arrives. A poll can
close the gate, but cannot open it: opening requires a matched callback and a
release after listener installation/recovery.

The native audio pipeline checks a 250 ms lease itself. The observer renews it
about every 20 ms; callback stalls, dead workers, and suspension cannot leave the
microphone latched indefinitely. Wall-clock leases include sleep, and backwards
clock changes fail closed. An unavailable/disabled tap, missing callbacks, a
locked/off-console macOS session, or an unavailable Windows input desktop cancels
PTT without its release tail. Recovery installs a fresh listener and requires a
release before another press. A UI status poll restarts a stale supervisor.
Delayed webview PTT commands cannot reopen native global capture.

Windows uses `WH_KEYBOARD_LL` or `WH_MOUSE_LL` on a dedicated message-pump thread.
Both callbacks **always return `CallNextHookEx`**, including unrelated events.
Injected events cannot open PTT. Windows can silently remove a slow hook; physical
release polling closes an existing hold, and a physical press without a matching
callback triggers reinstallation. See [Microsoft's low-level hook contract](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelkeyboardproc).
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
and lossless settings migration from v6 to v7. Browser screenshots exercise real
rendered settings with fixture permission states; they do not establish an OS
permission grant or physical keyboard/mouse behavior. Real macOS/Windows hardware
checks must still cover focused games/editors receiving the binding, mouse side
buttons, modifiers/AltGr, permission grant/revocation, sleep, lock/unlock, secure
input, and Windows hook eviction.
