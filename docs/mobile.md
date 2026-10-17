# Native mobile apps

Status: **early, Android first.** iOS follows once Android has voice.

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
channel, read and send channel chat. Kotlin gets a fresh `ServerView` on every
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
- Joining and leaving channels (presence only, no audio yet), including
  channels with a password.
- Plaintext channel chat: history with paging, live messages, sending. Every
  message is checked against its author's signature, as on the desktop.
  Messages from TeamSpeak are labeled.
- Light and dark themes following the system, in English and Polish.

## Not yet

Roughly in this order:

1. **Voice.** The `vc-client` engine already cross-compiles for Android
   (cpal uses AAudio there). Missing: the microphone permission, a foreground
   service so calls survive the screen turning off, audio focus, routing to
   Bluetooth and the earpiece, and latency measurements on real phones.
2. **Identity storage hardening.** Wrap the identity seed with a key from the
   Android Keystore.
3. **Encrypted chat and direct messages.** These need the MLS client logic
   that today lives in `apps/web` (TypeScript over `gwar-mls-wasm`), moved
   into the Rust core so every native app shares it. This waits until the
   main roadmap's encryption stages are finished.
4. Gwar Connect accounts, invites and links, attachments, server-wide chat,
   moderation, several servers, notifications.
5. iOS.

## Build and run

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

```bash
cargo run -p vc-server -- --data-dir /tmp/gwar-dev --http 127.0.0.1:8790 --insecure-any-host --no-connect
cargo run -p gwar-app --example peer -- ws://127.0.0.1:8790 Ola   # someone to talk to
```

In the app, enter `10.0.2.2` as the address and turn on "Allow unencrypted
LAN connection" under "More options".

Tests of the core run with the rest of the workspace
(`cargo test -p gwar-app`), including a session against a real in-process
server that drives the same API the app uses.
