# Desktop notifications and invitations

Audit follow-up for P2-14 through P2-17, checked against main `1a54b517`.

- P2-17 was implemented first, in a separate commit. Tray labels and tooltips follow English or Polish, use Gwar throughout, and show a mute slash or unread mark.
- P2-14 remained open except for message sounds, already delivered in #202. Server and channel menus now offer all messages, mentions only, none, and inheritance. Channel choices override server choices; otherwise existing global preferences apply. The same effective preferences gate existing message sounds, system notifications, desktop attention, and the desktop badge. A mute selected while signature verification is pending takes effect at notification delivery. A muted channel still keeps its messages and ordinary unread counters. DMs follow the server choice; mentions-only still allows DMs. Pokes and voice/presence cues retain their existing rules.
- P2-15 remained open. Installed macOS and Windows releases expose open-at-login and start-minimized switches, both off by default. The official Tauri autostart plugin owns OS registration. Opening settings reads actual OS state; only a successful native write and read-back changes the switches. Browser, debug, and Linux builds hide these controls. Linux native commands refuse changes and do not query the plugin's existence-only status. Saved Linux preferences are retained but not applied. Start minimized minimizes the window, preserving its Dock/taskbar entry; opening an invitation brings the review window forward. Window-state restoration retains geometry but cannot restore an old hidden state over this preference.
- P2-16 remained open. Installed bundles register `gwar://invite/`. A browser invitation offers an explicit desktop handoff on desktop operating systems. Cold and running-instance links use the same strict parser. Loading or receiving a link cannot join a server, contact its destination, or enter voice. Cold invite launches, including malformed links, suppress saved-session restoration. A running session stays connected until the person chooses to leave and review the new invitation, then separately clicks Join.

## Invitation identity and deployment boundary

New invitations include an optional server name supplied by the inviter. The destination address is always visible before joining, and details identify the name as inviter supplied and show the full server fingerprint. Old invitations display their destination instead. No speculative server name or member count is fetched. Authentication, expected-key pinning, owner checks, message signatures, and encrypted-message processing remain on their existing paths.

The desktop parser accepts only the invite route, one of each allowed parameter, a valid invite code, a full public-key fingerprint, an optional valid owner, a safe bounded display name, and a valid destination. It rejects credentials, fragments, unknown desktop parameters, control characters, duplicate values, unsafe schemes, and oversized input. Links are rendered as data; the browser's handoff is a normal user-clicked link.

No nginx or hosted website configuration was changed or deployed. The shared web invite screen's handoff button reaches the public site only through a separately approved normal website release. There is no HTTPS universal-link claim: existing HTTPS invitations first open their web page, where the person can click Open in the desktop app. macOS and Windows installers register the custom scheme. Linux releases refresh the scheme registration at startup, including AppImages moved to a new path; failed registration is logged without stopping the app. No runtime registration of a debug executable is performed.

## Persistence and capabilities

Settings version 7 adds `notificationOverrides` and the two desktop fields while preserving version 6 and earlier fields, including nested desktop shortcuts, notification choices, sounds, passwords, session state, and unknown top-level values. Existing fixtures remain intact and a version 7 fixture and explicit version 6 preservation regression cover the upgrade. If another settings migration merges first, renumber this migration and its fixture before merging.

The supported platforms' native window preference is also stored in app-config `startup.json`, version 1, because it must be read before creating the webview. A missing file means off; unreadable or newer files are retained and never overwritten. OS autostart registration is authoritative for open-at-login. Merely hydrating browser preferences never registers an executable.

No additional Tauri plugin capabilities are needed: the frontend drains raw launch arguments and a separate launch-intent flag through the app-owned `desktop_invite_links` command, and listens for queue wake events through the existing `core:event:allow-listen`. Autostart is mediated by app-owned boolean-only commands. Deep-link register/unregister and autostart plugin commands are not exposed to JavaScript. The navigation allowlist and webview media restrictions are unchanged.

## Dependency review

New locked dependencies and their published licenses:

| Crate | Version | License |
| --- | --- | --- |
| tauri-plugin-autostart | 2.7.0 | Apache-2.0 OR MIT |
| tauri-plugin-deep-link | 2.6.3 | Apache-2.0 OR MIT |
| auto-launch | 0.6.0 | MIT |
| dirs | 6.0.0 | MIT OR Apache-2.0 |
| nix | 0.31.3 | MIT |
| objc2-security | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| objc2-service-management | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| os_info | 3.15.0 | MIT |
| smappservice-rs | 0.1.3 | MIT |

Cargo metadata confirms these licenses are in the existing distribution allowlist. A refreshed `cargo audit` reported no entries in its vulnerability category and no advisories on new dependencies. Existing warnings remain for audiopus_sys, proc-macro-error, proc-macro-error2, zbus_names, zvariant, zvariant_derive (maintenance), and glib (RUSTSEC-2024-0429, soundness). This change does not update those dependencies.

References: [Tauri autostart](https://v2.tauri.app/plugin/autostart/), [Tauri deep linking](https://v2.tauri.app/plugin/deep-linking/).

## Verification

The browser regression script `apps/web/e2e/desktop-notifications.mjs` renders the actual shared UI in an isolated profile, blocks external HTTP requests, verifies notification selection and a compact invite row, and captures channel badges, the server menu, notification settings, startup settings on supported platforms and Linux, invitation details, and settings navigation at 1440, 768, 390, and 360 pixels. The startup screenshots use a display-only native status fixture, not OS registration. It never opens the installed desktop app.

Local validation passed:

- `pnpm --filter web typecheck`.
- `pnpm --filter web lint`: no errors; four warnings in unchanged files.
- Full Vitest: 152 files, 2,223 tests passed (`--maxWorkers=2 --testTimeout=20000` to avoid contention with concurrent builds). The delayed-verification mute regression was first observed failing, then passed after the fix. Warm and pasted-invite password regressions also passed.
- `pnpm --filter web build` (existing chunk-size and dynamic-import warnings).
- `CARGO_BUILD_JOBS=4 cargo fmt --all --check`.
- `CARGO_BUILD_JOBS=4 cargo clippy -p gwar-desktop --all-targets -- -D warnings`.
- `CARGO_BUILD_JOBS=4 cargo test -p gwar-desktop`: 56 passed, one existing signed-updater test ignored because it needs `GWAR_UPDATER_DIR`.
- The browser regression passed and 32 round-1 screenshots were visually inspected across all four widths. Gwar's existing palette, typography, navigation, and components were retained; the narrow invitation banner was shortened after screenshot inspection.

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
