# Native mobile apps

Status: **early, Android first.** iOS follows once Android voice has been
tried on real phones.

## An independent track, for now

Work on the mobile apps runs **independently of the main roadmap**. The
current stages (private channels, the key archive, the pre-release reviews and
the first release; see [e2e-build.md](e2e-build.md)) go on without waiting for
mobile, and mobile does not wait for them. Mobile builds only on what already
exists and is stable: the protocol, identity, the channel tree and plaintext
chat. The two tracks move in lockstep only after those stages are finished.
From then on a feature is done when every client has it.

Until then:

- Mobile work adds code (`crates/gwar-app`, `apps/android`) and does not
  change the shared crates' behaviour. If mobile needs something there, it
  goes in as a small, separate change.
- `crates/gwar-app` is a workspace member, so the main CI compiles, lints and
  tests it. A protocol change that breaks it gets the small fix it needs,
  usually mapping a new field. Mobile features themselves are not expected to
  keep up.
- The Android build (`.github/workflows/android.yml`) runs only when the
  mobile code, the client crates it builds on, or the lock file change.

## Native, with a shared Rust core

The mobile apps are native: Kotlin and Jetpack Compose on Android, Swift and
SwiftUI on iOS later. The desktop app will move to native UIs in the same way
later; until then it keeps the shared React UI.

The UI is written for each platform. Everything else lives once, in Rust:

```
apps/android (Kotlin, Compose)        apps/ios (Swift, later)
        │  UniFFI-generated bindings         │
        └──────────────┬─────────────────────┘
               crates/gwar-app       state of one server, address rules, key pins, storage
                       │
               crates/vc-client      handshake, signed chat, voice engine (cpal, Opus, str0m, SFrame)
               crates/vc-proto       the wire protocol
```

`gwar-app` is the shared core of the native apps. It turns server events into
a view (the channel tree with occupants, the people online but not in a
channel, unread counts) and exposes a small API: connect, join or leave a
channel, start and stop voice, mute and deafen, read and send channel chat.
It also keeps the connection alive: the server drops a client that sends
nothing for a minute, so the session pings every 20 s, as the web app does,
and an unanswered ping ends the session. Kotlin gets a fresh `ServerView` on every
change and renders it, so the app holds no protocol logic. Today the
equivalent logic for the web and desktop apps is TypeScript in `apps/web`;
the native desktop app will use `gwar-app` instead.

## What the Android app does today

- A device identity (Ed25519) created on first start and kept in the app's
  private storage, excluded from cloud backup and device transfer.
- Server addresses with the same rules as the desktop app: TLS by default,
  plaintext only to this device or through the explicit "unencrypted LAN"
  option.
- The server's key is pinned on first contact. A different key later is
  refused, with a warning and an explicit "trust the new key".
- The channel tree with who is where (muted, deafened, talking, away,
  TeamSpeak users), the people on the server who are in no channel, and unread
  counts that stay in sync with your other devices.
- Joining and leaving channels, including channels with a password.
- Voice in plaintext channels, on the same native engine as the desktop app
  (`vc-client`: 10 ms Opus, adaptive playout, WebRTC to the server's SFU).
  The microphone and speaker are opened through AAudio as a voice call
  (`VoiceCommunication` input preset and usage), so the phone's own echo
  cancellation and noise suppression apply. While voice runs, the phone is in
  communication mode with call audio focus, sound goes to a connected headset
  or else the loudspeaker (the output button picks another: loudspeaker,
  earpiece, wired, Bluetooth or USB headset; Bluetooth asks for its
  permission first), and a foreground service with an ongoing
  notification (with a Leave action) keeps the call going with the screen off.
  Mute and deafen are shown to everyone; a lost link or audio device restarts
  voice by itself. Tapping the voice status shows the engine's counters.
- Reconnecting by itself when the link drops: a ping timeout, a server
  restart or a change of network (Wi-Fi to mobile data and back is noticed
  at once, without waiting for the ping). It puts back the channel, voice,
  mute and deafen and the open chat; the call's service stays up meanwhile.
  It does not reconnect after being kicked, banned or removed, or when the
  same identity connected elsewhere, and it stops at a refusal or a changed
  server key, which the user has to see. The rules (which reasons, which
  errors, how long to wait) live in the core, for the other native apps.
- Plaintext channel chat: history with paging, live messages, sending. Every
  message is checked against its author's signature, as on the desktop.
  Messages from TeamSpeak are labeled.
- Light and dark themes following the system, in English and Polish.

## Not yet

Roughly in this order:

1. **Voice on real phones**: latency and loss measurements like the desktop's
   ([latency.md](latency.md)), output switching with real earpieces and
   Bluetooth headsets (the emulator has only a loudspeaker), push-to-talk.
2. **Identity storage hardening.** Wrap the identity seed with a key from the
   Android Keystore.
3. **Encrypted chat, encrypted voice and direct messages.** These need the MLS client logic
   that today lives in `apps/web` (TypeScript over `gwar-mls-wasm`), moved
   into the Rust core so every native app shares it. This waits until the
   main roadmap's encryption stages are finished.
4. Gwar Connect accounts, invites and links, attachments, server-wide chat,
   moderation, several servers, notifications.
5. iOS.

## Build and run

The app needs Android 9 (API 28) or newer: AAudio's voice call presets start
there.

Prerequisites: Android SDK with platform 37 and an NDK, JDK 17 or newer,
rustup with the Android target, `cargo-ndk` and CMake:

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_HOME=~/Library/Android/sdk   # wherever the SDK is
```

```bash
cd apps/android
./gradlew :app:assembleDebug                # also builds the Rust core (apps/android/build-rust.sh)
./gradlew :app:installDebug                 # onto a running emulator or a connected phone
./gradlew :app:assembleDebug -Pgwar.skipRust   # Kotlin-only changes: reuse the last Rust build
```

`build-rust.sh` builds `crates/gwar-app` with `cargo ndk` into
`app/build/rust/jniLibs` and generates the Kotlin bindings into
`app/build/rust/kotlin`. Nothing generated is committed.
`GWAR_ANDROID_ABIS="arm64-v8a x86_64"` adds the x86_64 emulator ABI;
`GWAR_RUST_PROFILE=dev` builds the core unoptimized.

Against a local server: the emulator reaches the host machine as `10.0.2.2`.
For voice, the server must announce a media address both the emulator and
the host can reach, such as the host's LAN address (`$LAN` below):

```bash
cargo run -p vc-server -- --data-dir /tmp/gwar-dev --http 127.0.0.1:8790 \
  --media $LAN:9997 --public-ip $LAN --insecure-any-host --no-connect
cargo run -p gwar-app --example peer -- ws://127.0.0.1:8790 Ola voice   # someone to talk to
```

In the app, enter `10.0.2.2` as the address and turn on "Allow unencrypted
LAN connection" under "More options". The peer answers chat messages; with
`voice` it also joins voice with a quiet 440 Hz tone and prints what it
hears. Start the emulator with audio (no `-no-audio`); its microphone is
silent unless host audio input is enabled, so the app receives but does not
send.

Tests of the core run with the rest of the workspace
(`cargo test -p gwar-app`), including sessions against a real in-process
server that drive the same API the app uses, voice included (a tone from one
session must reach the other, and muting must stop it).
`cargo test -p gwar-app -- --ignored` adds a 75 s check that an idle session
stays connected.
