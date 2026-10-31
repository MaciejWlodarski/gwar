# Desktop notifications and invitations

Audit follow-up for P2-14 through P2-17, checked against main `1a54b517`.

- P2-17 was implemented first, in a separate commit. Tray labels and tooltips follow English or Polish, use Gwar throughout, and show a mute slash or unread mark.
- P2-14 remained open except for message sounds, already delivered in #202. Server and channel menus now offer all messages, mentions only, none, and inheritance. Channel choices override server choices; otherwise existing global preferences apply. The same effective preferences gate existing message sounds, system notifications, desktop attention, and the desktop badge. A mute selected while signature verification is pending takes effect at notification delivery. A muted channel still keeps its messages and ordinary unread counters. DMs follow the server choice; mentions-only still allows DMs. Pokes and voice/presence cues retain their existing rules.
- P2-15 remained open. Installed macOS 13+ and Windows releases expose open-at-login and start-minimized switches, both off by default. Small app-owned adapters register a LaunchAgent on macOS and a quoted Run command on Windows. Opening settings reads effective OS state; only native read-back changes the switches. macOS approval or unconfirmed authorization keeps open-at-login off, with one compact status row. Settings opens only after a click, and returning to Gwar refreshes the status. Browser, debug, and Linux builds hide these controls. Linux native commands refuse changes. macOS before 13 also hides the startup controls because effective authorization cannot be queried there. Saved Linux preferences are retained but not applied. Start minimized minimizes the window, preserving its Dock/taskbar entry; opening an invitation brings the review window forward. Window-state restoration retains geometry but cannot restore an old hidden state over this preference.
- P2-16 remained open. Installed bundles register `gwar://invite/`. A browser invitation offers an explicit desktop handoff on desktop operating systems. Cold and running-instance links use the same strict parser. Loading or receiving a link cannot join a server, contact its destination, or enter voice. Cold invite launches, including malformed links, suppress saved-session restoration. A running session stays connected until the person chooses to leave and review the new invitation, then separately clicks Join.

## Invitation identity and deployment boundary

New invitations include an optional server name supplied by the inviter. The destination address is always visible before joining, and details identify the name as inviter supplied and show the full server fingerprint. Old invitations display their destination instead. No speculative server name or member count is fetched. Authentication, expected-key pinning, owner checks, message signatures, and encrypted-message processing remain on their existing paths.

The desktop parser accepts only the invite route, one of each allowed parameter, a valid invite code, a full public-key fingerprint, an optional valid owner, a safe bounded display name, and a valid destination. It rejects credentials, fragments, unknown desktop parameters, control characters, duplicate values, unsafe schemes, and oversized input. Links are rendered as data; the browser's handoff is a normal user-clicked link.

No nginx or hosted website configuration was changed or deployed. The shared web invite screen's handoff button reaches the public site only through a separately approved normal website release. There is no HTTPS universal-link claim: existing HTTPS invitations first open their web page, where the person can click Open in the desktop app. macOS and Windows installers register the custom scheme. Linux releases refresh the scheme registration at startup, including AppImages moved to a new path; failed registration is logged without stopping the app. No runtime registration of a debug executable is performed.

## Persistence and capabilities

Settings version 7 adds `notificationOverrides` and the two desktop fields while preserving version 6 and earlier fields, including nested desktop shortcuts, notification choices, sounds, passwords, session state, and unknown top-level values. Existing fixtures remain intact and a version 7 fixture and explicit version 6 preservation regression cover the upgrade. If another settings migration merges first, renumber this migration and its fixture before merging.

The supported platforms' native window preference is also stored in app-config `startup.json`, version 1, because it must be read before creating the webview. A missing file means off; unreadable or newer files are retained and never overwritten. OS autostart registration is authoritative for open-at-login. Merely hydrating browser preferences never registers an executable.

No additional Tauri plugin capabilities are needed: the frontend drains raw launch arguments and a separate launch-intent flag through the app-owned `desktop_invite_links` command, and listens for queue wake events through the existing `core:event:allow-listen`. Autostart is mediated by app-owned boolean-only commands. Deep-link register/unregister is not exposed to JavaScript. The app-owned login-settings command opens only the fixed macOS Login Items panel, from the main window. The navigation allowlist and webview media restrictions are unchanged.

## Dependency review

New locked dependencies retained from the initial implementation and their published licenses:

| Crate | Version | License |
| --- | --- | --- |
| tauri-plugin-deep-link | 2.6.3 | Apache-2.0 OR MIT |
| objc2-security | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-service-management | 0.3.2 | Zlib OR Apache-2.0 OR MIT |

Round 2 removes `tauri-plugin-autostart`, `auto-launch`, `smappservice-rs`, `os_info`, `nix 0.31.3`, and `dirs 6.0.0`. Nothing is vendored or patched. The replacement promotes already-locked `plist 1.10.1` (MIT), `windows-registry 0.6.1` (MIT OR Apache-2.0), and `objc2-service-management 0.3.2` to direct dependencies. No package or version is added to the round-1 lockfile.
Cargo metadata confirms these licenses are in the existing distribution allowlist. A refreshed `cargo audit` reported no entries in its vulnerability category and no advisories on new dependencies. Existing warnings remain for audiopus_sys, proc-macro-error, proc-macro-error2, zbus_names, zvariant, zvariant_derive (maintenance), and glib (RUSTSEC-2024-0429, soundness). This change does not update those dependencies.

References: [Apple legacy login-item authorization](https://developer.apple.com/documentation/servicemanagement/smappservice/statusforlegacyplist%28at%3A%29), [Tauri deep linking](https://v2.tauri.app/plugin/deep-linking/).

## Verification

The browser regression script `apps/web/e2e/desktop-notifications.mjs` renders the actual shared UI in an isolated profile, blocks external HTTP requests, verifies notification selection and a compact invite row, and captures channel badges, the server menu, notification settings, startup settings on supported platforms and Linux, invitation details, and settings navigation at 1440, 768, 390, and 360 pixels. The startup screenshots use a display-only native status fixture, not OS registration. It never opens the installed desktop app.

Local validation passed:

- `pnpm --filter web typecheck`.
- `pnpm --filter web lint`: no errors; four warnings in unchanged files.
- Full Vitest: 152 files, 2,227 tests passed (`--maxWorkers=2 --testTimeout=20000` to avoid contention with concurrent builds). The delayed-verification mute regression was first observed failing, then passed after the fix. Warm and pasted-invite password regressions also passed.
- `pnpm --filter web build` (existing chunk-size and dynamic-import warnings).
- `CARGO_BUILD_JOBS=4 cargo fmt --all --check`.
- `CARGO_BUILD_JOBS=4 cargo clippy -p gwar-desktop --all-targets -- -D warnings`.
- `CARGO_BUILD_JOBS=4 cargo test -p gwar-desktop`: 60 passed, one existing signed-updater test ignored because it needs `GWAR_UPDATER_DIR`.
- The browser regression passed and 48 round-2 screenshots were refreshed across all four widths; the 24 affected startup screenshots were visually inspected. Gwar's existing palette, typography, navigation, and components were retained; the narrow invitation banner was shortened after screenshot inspection.

Manual release acceptance still needs installed-platform checks of login registration, login launch and minimization on macOS and Windows, plus cold/warm OS URL dispatch and native tray/taskbar rendering on macOS, Windows, and Linux. Unit and browser tests do not establish those OS behaviors. Production Gwar and TeamSpeak were never contacted, and `/Applications/Gwar.app` was never launched.


## Review round 1

- S1: raw `gwar:` command-line intent is recorded before any URL parser runs. The native reply keeps a sticky `launched` flag separate from the bounded raw link queue, so malformed or oversized input suppresses saved-session restoration even if no usable URL remains. A native malformed-port regression and the frontend empty-link-list regression cover this.
- S2: staging an invitation clears the previous password/error prompts and close reason. A changed invitation also clears the connect form's local password before submission. Warm and pasted-link regressions first prove the old password was captured, then assert the invited server receives an empty password and no stale prompt remains.
- S3: an `Arc<Mutex<...>>` queue exists before plugin setup. The single-instance callback records raw arguments independently of DeepLink state; its optional plugin forwarding feature is disabled. Native OS-open listeners and the macOS initial snapshot are installed before webview creation. The renderer listens before an atomic drain and serializes later drains. A second-launch test records arguments from another thread before any app or plugin exists and then drains them. A mock-app test invokes the exact single-instance callback without DeepLink state: it failed with the former focus-only callback, then passed after forwarding was restored. A browser unit test covers a wake arriving during the initial read.
- S4/S5: the accepted fallback is used: both startup switches are hidden on Linux, native startup commands refuse changes, and neither the broken executable-path writer nor the existence-only XDG status reader is called. Both Linux policy regressions failed under the old platform policy, then passed. Unsupported status replies preserve saved preferences. Linux support requires a future quoted desktop-entry implementation with effective XDG disable/precedence handling.
- Settings pages are now Głos / Dźwięki aplikacji and Voice / App sounds. Shared translation keys update navigation, headings, and recovery links; smoke selectors and language regressions use the new names. PR #207 may need label conflict resolution if it merges first.

CI run [41174201036](https://github.com/MaciejWlodarski/gwar/actions/runs/41174201036), at head `5a78c1b1`, was inspected directly. The Windows media step compiled for 1m43s, passed all 53 tests, and hit its two-minute deadline as doctests started. Its timeout now allows a cold build. The Rust workspace test step spent 9m04s compiling before running tests and was still completing server screen simulations at its 15-minute deadline. That command excludes `gwar-desktop`, so it never runs the new single-instance/deep-link plugin code. Compilation now has its own bounded Build tests step; test execution keeps its separate 15-minute deadline and the job keeps its 40-minute total deadline.

The browser-smoke failure was an outdated URL-shape assertion: the invite now includes `name`. The test also expected the retired invitation copy and Connect label. It now validates every expected link field, the inviter's server name and verified fingerprint, waits for the invitation review screen, asserts no WebSocket opens before the explicit Join click, and retains admission and the invite-role color assertion for the new Ivy identity.


Round-1 local validation passed: typecheck; lint (the same four warnings in unchanged files); all 2,223 Vitest tests across 152 files; cargo fmt; desktop Clippy with warnings denied; desktop tests (56 passed, one existing updater-fixture test ignored); all 53 platform-neutral media tests and doctests; the 11 CI-selection regression tests; and the full loopback browser smoke suite (72/72 checks). The invite admission/role assertion passed after the link-field, invitation-copy and explicit-Join selectors were updated. Mobile Voice settings selectors target the settings row rather than the live voice-status control. The admission/role assertion was retained; no tests were skipped to obtain the pass.

The responsive browser suite passed with 32 screenshots at 1440, 768, 390 and 360 px, including the Linux policy and both renamed settings pages. All were visually inspected. The round-1 screenshot output is kept separately from the original review images. No production services or installed desktop application were used.


## Review round 2

- S1: Windows Run values contain exactly one quoted executable, including paths with spaces, ampersands, or Unicode. Quotes, NUL, and line breaks are refused rather than interpolated. The adapter uses the registry API, not a shell. Existing machine-wide entries from the former plugin remain in their original hive; write failures are reported rather than silently ignoring the old entry. Task Manager's startup approval is read, and an explicit enable updates it. The spaced-path regression failed with the former unquoted format. A Windows-only test writes and reads the same command through an isolated temporary registry key, never the actual Run key.
- S2: macOS calls `SMAppService.statusForLegacyPlist(at:)` through its Objective-C selector `statusForLegacyURL:`. A plist on disk is insufficient: only `Enabled` checks the switch. `RequiresApproval` shows a short Polish or English line and an explicit System Settings button. Unknown/not-yet-discovered registration stays off with a compact status/check-again row. No polling, automatic Settings opening, consent override, or second app launch is performed. Returning from Settings re-reads authorization. The existence-only projection and missing approval UI were first observed failing their regressions.
- S3: `plist::Value` serializes LaunchAgent XML and round-trip validation refuses any unrepresentable path before touching the installed entry. A regression containing `&`, `<`, `>`, quotes, apostrophes, and Polish characters failed with the former raw XML formatting. Writes use an atomic rename and retain the former `Gwar.plist` filename/Label to avoid duplicate entries. Native `startup.json` and web settings versions are unchanged; their saved preferences are retained.

Round-2 verification distinguishes host evidence from installed-platform acceptance. The Mac compiles the actual Objective-C bridge and runs the authorization projection, XML round-trip, and shared Windows quoting regressions. Browser screenshots use display-only status replies, so they establish layout and behavior rather than OS authorization. No real login registration or background-item permission was changed on this Mac, and no installed Gwar application was launched. A signed installed Mac build still needs a System Settings disable/approve cycle, a special-character install-path test, and a login launch. The actual Windows registration source and its tests cross-compile for `x86_64-pc-windows-gnu` in an isolated harness with a minimal Tauri handle stub, using the already-locked package versions. This checks Windows API types, not the complete Tauri Windows build or runtime behavior. The isolated registry test, Task Manager enable/disable, and actual spaced-path login launch still need a real Windows machine. Linux remains hidden.

Refreshed screenshots are in `~/dev/projects/gwar-mockups/desktop-notifications/round-2/`, at 1440, 768, 390, and 360 pixels. The regression checks approval and pending rows in both languages, including single-line fit, and blocks external requests.

Round-2 local checks passed: typecheck; lint with zero errors and the same four existing warnings; full Vitest (152 files, 2,227 tests); cargo fmt; desktop Clippy with warnings denied; full desktop crate tests (60 passed, one existing signed-updater fixture test ignored); isolated Windows GNU compilation of the real adapter and its tests; and the responsive browser regression. The cached `cargo audit --no-fetch` check reported zero vulnerabilities, with only the previously documented maintenance/soundness warnings. All Cargo commands used `CARGO_BUILD_JOBS=4`.
